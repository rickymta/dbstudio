//! Tests of the flavour specific parts: TiDB, `ANSI_QUOTES` following `sql_mode`, and R6 (the
//! certificate name checked behind an SSH tunnel). Run with `DBSTUDIO_IT=1`.
//!
//! Probe-then-skip: without the TiDB / `sshd` containers a test prints "skip" and passes, unless
//! `DBSTUDIO_IT_STRICT=1`.

mod common;

use std::sync::Arc;

use common::*;
use dbstudio_core::{
    DdlOptions, DdlRequest, DdlSection, Driver, Error, ObjectKind as K, ObjectPath, PlanFormat,
    QueryRequest, SchemaModelRequest, ScriptDialect, Session, SshAuth, SshConfig, TlsMode,
    execute_collect, folder,
};
use dbstudio_driver_mysql::{MySqlDriver, TiDbDriver};
use dbstudio_ssh::{MemoryKnownHosts, Tunnel};

fn skip(service: &str, why: &str) {
    assert!(
        !dbstudio_core::testing::it_strict_for(service),
        "DBSTUDIO_IT_STRICT covers {service} but {why}"
    );
    println!("skip: {why}");
}

fn db(name: &str) -> ObjectPath {
    ObjectPath::root().child(K::Database, name)
}

// ---------------------------------------------------------------------------------------------
// Flavour detection

#[tokio::test]
async fn flavour_follows_the_server_not_the_profile_kind() {
    if !it_enabled() {
        return;
    }
    if selected(TIDB) {
        // A profile that says "mysql" but points at TiDB gets the TiDB flavour.
        let s = user_session(TIDB).await;
        let info = s.server_info().await.unwrap();
        assert_eq!(
            (info.product.as_str(), info.extra["flavor"].as_str()),
            ("TiDB", "tidb")
        );
        assert_eq!(info.extra["readOnlyEnforcement"], "application");
        assert!(info.extra["readOnlyNote"].contains("guard"));
        let caps = s.capabilities().expect("TiDB overrides the descriptor");
        assert!(
            caps.sequences && !caps.triggers && !caps.stored_routines && !caps.read_only_session
        );
        // ... and the TiDB driver on the same server behaves identically.
        let s2 = TiDbDriver::new()
            .connect(&config(TIDB, "dbstudio", TlsMode::Disable), Some(&secret()))
            .await
            .unwrap();
        assert_eq!(s2.server_info().await.unwrap().product, "TiDB");
    } else {
        skip("tidb", "tidb is not running");
    }
    if selected(MYSQL8) {
        // The TiDB driver pointed at a MySQL server falls back to the MySQL flavour.
        let s = TiDbDriver::new()
            .connect(
                &config(MYSQL8, "dbstudio", TlsMode::Disable),
                Some(&secret()),
            )
            .await
            .unwrap();
        let info = s.server_info().await.unwrap();
        assert_eq!(
            (info.product.as_str(), info.extra["flavor"].as_str()),
            ("MySQL", "mysql")
        );
        assert!(s.capabilities().is_none());
    }
}

// ---------------------------------------------------------------------------------------------
// TiDB objects

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequences_are_listed_and_have_ddl() {
    if !it_enabled() {
        return;
    }
    for t in [TIDB, MARIADB].into_iter().filter(|t| selected(*t)) {
        let dbn = scratch_db(t, "seq").await;
        let root = root_session(t).await;
        run(
            &*root,
            &format!("CREATE SEQUENCE `{dbn}`.`s1` START WITH 5 INCREMENT BY 2"),
        )
        .await;
        let s = user_session(t).await;
        let folders = s.list_children(&db(&dbn)).await.unwrap();
        let f = folders
            .iter()
            .find(|n| n.name == folder::SEQUENCES)
            .unwrap_or_else(|| panic!("[{}] no sequences folder: {folders:?}", t.name));
        assert_eq!(f.detail.as_deref(), Some("1"));
        let seqs = s.list_children(&f.path).await.unwrap();
        assert_eq!(seqs.len(), 1);
        assert_eq!((seqs[0].kind, seqs[0].name.as_str()), (K::Sequence, "s1"));
        let ddl = s.object_ddl(&seqs[0].path).await.unwrap();
        assert!(
            ddl.to_uppercase().contains("CREATE SEQUENCE"),
            "[{}] {ddl}",
            t.name
        );
        // NEXTVAL works through a lease like any statement.
        let rs = query(&*s, &format!("SELECT NEXTVAL(`{dbn}`.`s1`)")).await;
        assert_eq!(ints(&rs, 0), vec![5], "[{}]", t.name);
        root.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tidb_has_no_routine_trigger_or_event_folders() {
    if !it_enabled() || !selected(TIDB) {
        return;
    }
    ensure_fixture(TIDB).await;
    let s = user_session(TIDB).await;
    let folders = s.list_children(&db(FIXTURE_DB)).await.unwrap();
    let names: Vec<_> = folders.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, ["tables", "views"]);
    let path = db(FIXTURE_DB)
        .child(K::Folder, folder::TABLES)
        .child(K::Table, "child");
    let tf = s.list_children(&path).await.unwrap();
    assert_eq!(
        tf.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
        ["columns", "indexes"]
    );
    // Routine and trigger paths never exist.
    for p in [
        db(FIXTURE_DB).child(K::Function, "f"),
        db(FIXTURE_DB).child(K::Procedure, "p"),
        db(FIXTURE_DB)
            .child(K::Table, "child")
            .child(K::Trigger, "t"),
    ] {
        assert!(
            matches!(s.object_ddl(&p).await, Err(Error::NotFound(_))),
            "{p:?}"
        );
    }
}

/// Model and DDL generation on a TiDB friendly schema (the Sakila fixture of `it_erd` needs
/// FULLTEXT / routines / triggers): the DDL replays into an empty database.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_and_ddl_replay_on_tidb() {
    if !it_enabled() || !selected(TIDB) {
        return;
    }
    let t = TIDB;
    let _a = drop_guard(t, "tidb_it_model");
    let _b = drop_guard(t, "tidb_it_model_copy");
    let root = root_session(t).await;
    run(
        &*root,
        &format!(
            "DROP DATABASE IF EXISTS tidb_it_model; CREATE DATABASE tidb_it_model{ci};
             DROP DATABASE IF EXISTS tidb_it_model_copy; CREATE DATABASE tidb_it_model_copy{ci}",
            ci = ci_collation(t)
        ),
    )
    .await;
    let mut q = QueryRequest::new(
        "CREATE TABLE country (id INT PRIMARY KEY AUTO_INCREMENT, name VARCHAR(40) NOT NULL, \
            UNIQUE KEY uq_name (name)) COMMENT='Countries';
         CREATE TABLE city (id BIGINT PRIMARY KEY AUTO_RANDOM, country_id INT NOT NULL, \
            name VARCHAR(40), pop INT DEFAULT 0, KEY idx_country (country_id), \
            CONSTRAINT fk_city_country FOREIGN KEY (country_id) REFERENCES country(id) \
            ON DELETE CASCADE);
         CREATE VIEW v_city AS SELECT id, name FROM city;
         CREATE SEQUENCE seq_a START WITH 10;",
    );
    q.database = Some("tidb_it_model".into());
    execute_collect(&*root, q).await.unwrap();

    let model = root
        .schema_model(SchemaModelRequest {
            scope: db("tidb_it_model"),
            tables: vec![],
            include_views: true,
            include_comments: true,
            detail: dbstudio_core::ModelDetail::Full,
            sample_size: None,
        })
        .await
        .unwrap();
    let names: Vec<_> = model.tables.iter().map(|x| x.name.as_str()).collect();
    assert!(
        names.contains(&"country") && names.contains(&"city"),
        "{names:?}"
    );
    assert_eq!(model.relations.len(), 1, "{:?}", model.relations);
    let country = model.tables.iter().find(|x| x.name == "country").unwrap();
    assert_eq!(country.comment.as_deref(), Some("Countries"));
    assert_eq!(country.unique_keys, vec![vec!["name".to_string()]]);

    let req = DdlRequest {
        scope: db("tidb_it_model"),
        objects: vec![],
        options: DdlOptions {
            qualify_schema: false,
            ..DdlOptions::default()
        },
    };
    let items = root.generate_ddl(req.clone()).await.unwrap();
    assert!(
        items
            .iter()
            .any(|i| i.sql.contains("ADD CONSTRAINT") && i.section == DdlSection::PostData)
    );
    // The sequence is part of the DDL and comes before the tables.
    let seq = items
        .iter()
        .position(|i| i.kind == K::Sequence)
        .expect("sequence item");
    let first_table = items.iter().position(|i| i.kind == K::Table).unwrap();
    assert!(seq < first_table);
    assert!(
        items[seq].sql.starts_with("CREATE SEQUENCE `seq_a`"),
        "{}",
        items[seq].sql
    );
    for i in &items {
        let mut q = QueryRequest::new(i.sql.clone());
        q.database = Some("tidb_it_model_copy".into());
        execute_collect(&*root, q)
            .await
            .unwrap_or_else(|e| panic!("replay failed: {e}\n{}", i.sql));
    }
    let copy = root
        .schema_model(SchemaModelRequest {
            scope: db("tidb_it_model_copy"),
            tables: vec![],
            include_views: true,
            include_comments: true,
            detail: dbstudio_core::ModelDetail::Full,
            sample_size: None,
        })
        .await
        .unwrap();
    let summary = |m: &dbstudio_core::SchemaModel| {
        let mut v: Vec<(String, usize)> = m
            .tables
            .iter()
            .map(|x| (x.name.clone(), x.columns.len()))
            .collect();
        v.sort();
        v
    };
    assert_eq!(summary(&model), summary(&copy));
    assert_eq!(model.relations.len(), copy.relations.len());
    root.close().await.unwrap();
}

/// EXPLAIN [ANALYZE] on TiDB: rows packed as JSON, parsed by `dbstudio-plan`, nothing is
/// written by an analysed SELECT.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explain_packs_rows_for_the_tidb_parser() {
    if !it_enabled() || !selected(TIDB) {
        return;
    }
    ensure_fixture(TIDB).await;
    let s = user_session(TIDB).await;
    for analyze in [false, true] {
        let raw = s
            .explain(dbstudio_core::ExplainRequest {
                query_id: uuid::Uuid::new_v4(),
                text: "SELECT grp, COUNT(*) FROM big WHERE payload LIKE 'row-1%' GROUP BY grp"
                    .into(),
                database: Some(FIXTURE_DB.into()),
                analyze,
                options: dbstudio_core::ExplainOptions::default(),
                timeout_ms: None,
            })
            .await
            .unwrap();
        assert_eq!(raw.format, PlanFormat::TiDbRows);
        assert_eq!((raw.analyzed, raw.rolled_back), (analyze, analyze));
        assert!(
            raw.server_version.contains("TiDB"),
            "{}",
            raw.server_version
        );
        let tree = dbstudio_plan::parse_and_analyze(&raw, &dbstudio_plan::PlanRules::default())
            .unwrap_or_else(|e| panic!("{e}\n{}", raw.text));
        assert_eq!(tree.analyzed, analyze);
        let mut stack = vec![&tree.root];
        let mut full = false;
        while let Some(n) = stack.pop() {
            full |= n.kind == dbstudio_plan::PlanNodeKind::FullScan;
            stack.extend(n.children.iter());
        }
        assert!(full, "expected a TableFullScan:\n{}", raw.text);
        if analyze {
            assert!(tree.totals.execution_time_ms.is_some());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// ANSI_QUOTES follows sql_mode (all flavours)

fn ansi_of(s: &dyn Session) -> bool {
    match s.script_dialect() {
        ScriptDialect::MySql { ansi_quotes, .. } => ansi_quotes,
        other => panic!("not a MySQL dialect: {other:?}"),
    }
}

fn lease_ansi(l: &dyn dbstudio_core::Lease) -> bool {
    match l
        .script_dialect()
        .expect("MySQL leases report their dialect")
    {
        ScriptDialect::MySql { ansi_quotes, .. } => ansi_quotes,
        other => panic!("not a MySQL dialect: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ansi_quotes_follow_the_lease_sql_mode() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        // 5.7 is emulated and slow; the logic is the same as on 8.
        if t.name == "mysql57" {
            continue;
        }
        let db = scratch_db(t, "ansi").await;
        let s = user_session(t).await;
        assert!(
            !ansi_of(&*s),
            "[{}] default sql_mode has no ANSI_QUOTES",
            t.name
        );
        let lease = s
            .open_lease(dbstudio_core::LeaseOptions {
                database: Some(db.to_string()),
                schema: None,
                auto_commit: true,
            })
            .await
            .unwrap();
        let run = |sql: &'static str| {
            let lease = &lease;
            async move {
                let mut sink = dbstudio_core::CollectingSink::new();
                lease
                    .execute(QueryRequest::new(sql), &mut sink)
                    .await
                    .unwrap();
            }
        };
        run("SET SESSION sql_mode = 'ANSI_QUOTES'").await;
        assert!(
            lease_ansi(&*lease),
            "[{}] ANSI_QUOTES is on after SET",
            t.name
        );
        assert!(
            !ansi_of(&*s),
            "[{}] the session dialect is not changed by a lease",
            t.name
        );
        // `script_analyze` reports it in the dialect.
        let analysis =
            dbstudio_sql::analyze(lease.script_dialect().unwrap(), "SELECT \"a\" FROM t");
        let json = serde_json::to_value(analysis.dialect).unwrap();
        assert_eq!(json["ansiQuotes"], true, "[{}] {json}", t.name);
        run("SET SESSION sql_mode = 'ANSI'").await;
        assert!(lease_ansi(&*lease), "[{}] ANSI implies ANSI_QUOTES", t.name);
        run("SET SESSION sql_mode = ''").await;
        assert!(!lease_ansi(&*lease), "[{}]", t.name);
        // A statement that merely mentions the word does not change anything.
        run("SELECT 'sql_mode'").await;
        assert!(!lease_ansi(&*lease), "[{}]", t.name);
        lease.close().await.unwrap();
    }
}

#[tokio::test]
async fn ansi_quotes_from_the_init_command_are_seen_at_connect() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        if t.name == "mysql57" {
            continue;
        }
        let mut c = config(t, "dbstudio", TlsMode::Disable);
        c.options.insert(
            "init_command".into(),
            "SET SESSION sql_mode = 'ANSI_QUOTES'".into(),
        );
        let s = connect_cfg(&c).await.unwrap();
        assert!(ansi_of(&*s), "[{}]", t.name);
    }
}

// ---------------------------------------------------------------------------------------------
// R6: the verified host name behind an SSH tunnel

async fn open_tunnel(target: (&str, u16)) -> Option<Tunnel> {
    let ssh_port: u16 = std::env::var("DBSTUDIO_IT_SSH_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(22022);
    let reachable = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], ssh_port)),
        std::time::Duration::from_secs(1),
    )
    .is_ok();
    if !reachable {
        skip("sshd", "sshd is not running");
        return None;
    }
    let cfg = SshConfig {
        host: "127.0.0.1".into(),
        port: ssh_port,
        username: "dbstudio".into(),
        auth: SshAuth::Password,
        keepalive_secs: Some(15),
        connect_timeout_ms: Some(15_000),
    };
    let secret = dbstudio_core::Secret::new("dbstudio_ssh");
    let known = MemoryKnownHosts::new();
    let target = (target.0.to_string(), target.1);
    // The first attempt reports the host key; trust it (like the application's prompt) and retry.
    let Err(Error::SshHostKey {
        host,
        port,
        key_type,
        fingerprint,
        ..
    }) = Tunnel::open(&cfg, Some(&secret), target.clone(), &known).await
    else {
        panic!("expected the host key prompt");
    };
    known.trust(&host, port, &key_type, &fingerprint);
    Some(
        Tunnel::open(&cfg, Some(&secret), target, &known)
            .await
            .expect("tunnel"),
    )
}

/// `verify-full` through a tunnel: the TCP host is `127.0.0.1`, the certificate is checked
/// against `tls.server_name` (what the application sets to the original host).
#[tokio::test]
async fn r6_verify_full_checks_the_server_name_not_the_tunnel_address() {
    if !it_enabled() {
        return;
    }
    if !selected(MYSQL8) {
        skip("mysql8", "mysql8 is not selected");
        return;
    }
    let Some(tunnel) = open_tunnel(("mysql8", 3306)).await else {
        return;
    };
    let mk = |server_name: Option<&str>| {
        let mut c = config(MYSQL8, "dbstudio", TlsMode::VerifyFull);
        c.host = "127.0.0.1".into();
        c.port = tunnel.local_addr().port();
        c.tls.ca_cert_path = Some(certs_dir().join("ca.crt").to_string_lossy().into_owned());
        c.tls.server_name = server_name.map(str::to_string);
        c
    };
    // A name in the certificate: connects, encrypted and verified.
    let s = connect_cfg(&mk(Some("localhost")))
        .await
        .expect("server_name = localhost");
    assert_eq!(s.server_info().await.unwrap().extra["tls"], "on");
    // A name that is NOT in the certificate is refused although `127.0.0.1` (the TCP host)
    // is: proves the override is what gets verified.
    match connect_cfg(&mk(Some("wrong.example"))).await {
        Err(Error::Tls(m)) => assert!(m.contains("wrong.example"), "{m}"),
        Err(e) => panic!("expected a TLS error, got {e:?}"),
        Ok(_) => panic!("connected although the server name is not in the certificate"),
    }
    // Without an override the TCP host (in the certificate) is used, as before.
    connect_cfg(&mk(None)).await.expect("no override");
    // The same through the TiDB-flavoured driver entry point (shared code path).
    let s: Arc<dyn Session> = Arc::from(
        TiDbDriver::new()
            .connect(&mk(Some("localhost")), Some(&secret()))
            .await
            .unwrap(),
    );
    s.ping().await.unwrap();
    let _ = MySqlDriver::new();
}

/// Two leases of one session with different `sql_mode`: each reports its own dialect, so a
/// guard that uses it splits exactly like the lease (the attack text is ONE string in the
/// ANSI_QUOTES lease's view of `"..."` only when quotes are identifiers; here the attack hides a
/// DELETE from a splitter using the wrong mode).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_lease_reports_its_own_dialect_and_the_guard_sees_the_delete() {
    if !it_enabled() {
        return;
    }
    let t = MYSQL8;
    if !selected(t) {
        return;
    }
    let db = scratch_db(t, "dialect_leases").await;
    let s = user_session(t).await;
    let open = || async {
        s.open_lease(dbstudio_core::LeaseOptions {
            database: Some(db.to_string()),
            schema: None,
            auto_commit: true,
        })
        .await
        .unwrap()
    };
    let (a, b) = (open().await, open().await);
    let mut sink = dbstudio_core::CollectingSink::new();
    a.execute(
        QueryRequest::new("SET SESSION sql_mode = 'ANSI_QUOTES,NO_BACKSLASH_ESCAPES'"),
        &mut sink,
    )
    .await
    .unwrap();
    assert!(lease_ansi(&*a));
    assert!(!lease_ansi(&*b), "lease B keeps its own sql_mode");
    // The attack: with ANSI_QUOTES a double-quoted text is an identifier and `\` is a plain
    // character, so the lexer of lease A sees `"\"` as a complete identifier and `DELETE` as
    // a second statement; the default dialect (session / lease B) reads one string.
    let attack = "SELECT \"\\\"; DELETE FROM t; -- \"";
    let da = a.script_dialect().unwrap();
    let db_ = b.script_dialect().unwrap();
    let kinds = |d| {
        dbstudio_sql::analyze(d, attack)
            .statements
            .iter()
            .map(|x| x.class)
            .collect::<Vec<_>>()
    };
    assert!(
        kinds(da).contains(&dbstudio_core::StatementClass::Write),
        "the guard with lease A's dialect must see the DELETE: {:?}",
        kinds(da)
    );
    assert!(
        !kinds(db_).contains(&dbstudio_core::StatementClass::Write),
        "the default dialect reads one string (this is why the wrong dialect was dangerous)"
    );
    a.close().await.unwrap();
    b.close().await.unwrap();
}
