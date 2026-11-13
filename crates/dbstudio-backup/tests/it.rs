//! Integration tests against the Docker databases (`DBSTUDIO_IT=1`): integrated backup ->
//! restore into an empty database must give identical rows; native tools run through shims
//! that forward to `docker exec` (the host has no `pg_dump` / `mysqldump`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dbstudio_backup::{
    BackupContext, BackupEngine, BackupSpec, Compression, Destination, NativeTool, RestoreSpec,
    inspect, run_backup, run_restore, verify,
};
use dbstudio_core::{
    CancelToken, ConnectionConfig, DbKind, Driver, JobProgress, JobReporter, ObjectKind,
    ObjectPath, QueryRequest, Secret, Session, TableDataRequest, TlsMode, Value, execute_collect,
};
use dbstudio_export::ExportContent;
use uuid::Uuid;

fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").as_deref() == Ok("1")
}

#[derive(Default)]
struct Rep {
    cancel: CancelToken,
    phases: std::sync::Mutex<Vec<String>>,
}

impl JobReporter for Rep {
    fn progress(&self, p: &JobProgress) {
        let mut v = self.phases.lock().unwrap();
        if v.last() != Some(&p.phase) {
            v.push(p.phase.clone());
        }
    }
    fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }
}

fn unique() -> String {
    Uuid::new_v4().simple().to_string()[..10].to_string()
}

fn config(kind: DbKind) -> ConnectionConfig {
    let mut c = ConnectionConfig::new(kind, "it-conn", "127.0.0.1");
    c.port = if kind == DbKind::Postgres {
        54316
    } else {
        33068
    };
    c.username = Some("dbstudio".into());
    c.database = Some("dbstudio".into());
    c.tls.mode = TlsMode::Disable;
    c.connect_timeout_ms = Some(5000);
    c
}

async fn connect(kind: DbKind) -> (ConnectionConfig, Arc<dyn Session>) {
    let cfg = config(kind);
    let secret = Secret::new("dbstudio_test");
    let session: Box<dyn Session> = match kind {
        DbKind::Postgres => dbstudio_driver_postgres::PostgresDriver::new()
            .connect(&cfg, Some(&secret))
            .await
            .unwrap(),
        _ => dbstudio_driver_mysql::MySqlDriver::new()
            .connect(&cfg, Some(&secret))
            .await
            .unwrap(),
    };
    (cfg, Arc::from(session))
}

async fn exec(s: &dyn Session, db: Option<&str>, sql: &str) {
    let mut req = QueryRequest::new(sql);
    req.database = db.map(str::to_string);
    execute_collect(s, req)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn ctx(
    cfg: &ConnectionConfig,
    s: &Arc<dyn Session>,
    rep: &Arc<Rep>,
    tools: BTreeMap<NativeTool, String>,
) -> BackupContext {
    BackupContext {
        session: s.clone(),
        config: cfg.clone(),
        secret: Some(Secret::new("dbstudio_test")),
        tunnel_addr: None,
        reporter: rep.clone(),
        job_name: Some("it".into()),
        tool_overrides: tools,
    }
}

fn spec(db: &str, encrypted: bool, dir: &Path, engine: BackupEngine) -> BackupSpec {
    BackupSpec {
        connection_id: Uuid::nil(),
        targets: vec![ObjectPath::root().child(ObjectKind::Database, db)],
        engine,
        compression: Compression::Zstd { level: 3 },
        encrypted,
        destination: Destination::LocalDir {
            path: dir.to_string_lossy().into_owned(),
            file_template: "{database}-{date:%H%M%S}.dbsbak".into(),
        },
    }
}

fn logical() -> BackupEngine {
    BackupEngine::Logical {
        content: ExportContent::Both,
        ddl: Default::default(),
    }
}

/// Every row of a table, ordered by the first column.
async fn rows_of(s: &dyn Session, table: ObjectPath) -> Vec<Vec<Value>> {
    let mut sink = dbstudio_core::CollectingSink::new();
    s.fetch_table(
        TableDataRequest {
            query_id: Uuid::new_v4(),
            table,
            filters: vec![],
            raw_where: None,
            sort: vec![],
            offset: 0,
            limit: 1_000_000,
            batch_size: None,
        },
        &mut sink,
    )
    .await
    .unwrap();
    let mut rows = match sink.into_results().remove(0).outcome {
        dbstudio_core::StatementOutcome::Rows(r) => r.rows,
        other => panic!("{other:?}"),
    };
    rows.sort_by_key(|r| format!("{:?}", r[0]));
    rows
}

fn pg_table(db: &str, schema: &str, t: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Database, db)
        .child(ObjectKind::Schema, schema)
        .child(ObjectKind::Table, t)
}

fn my_table(db: &str, t: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Database, db)
        .child(ObjectKind::Table, t)
}

const PG_FIXTURE: &[&str] = &[
    "CREATE SCHEMA app",
    "CREATE TABLE app.customers (id serial PRIMARY KEY, name text NOT NULL, email text UNIQUE, \
     created timestamptz DEFAULT '2026-01-02 03:04:05+00', tags text[], meta jsonb, avatar bytea, \
     balance numeric(12,2), active boolean)",
    "CREATE TABLE app.orders (id bigserial PRIMARY KEY, customer_id int REFERENCES app.customers(id), \
     total double precision, note text)",
    "CREATE INDEX orders_customer ON app.orders (customer_id)",
    "CREATE VIEW app.big_orders AS SELECT * FROM app.orders WHERE total > 100",
    "CREATE SEQUENCE app.standalone START 50",
    "INSERT INTO app.customers (name, email, tags, meta, avatar, balance, active) VALUES \
     ('Ann ''quoted''; \"dq\" \\ back', 'a@x', ARRAY['x','y z','q\"r'], '{\"k\": [1, 2]}', '\\xdeadbeef', 12.50, true), \
     ('Bé Ünïcode 日本', NULL, NULL, NULL, NULL, NULL, false), \
     ('Line\nbreak', 'c@x', ARRAY[]::text[], 'null', '\\x', -0.01, NULL)",
    "INSERT INTO app.orders (customer_id, total, note) SELECT (g % 3) + 1, g * 1.5, 'n' || g \
     FROM generate_series(1, 5300) g",
    "SELECT nextval('app.standalone')",
    "SELECT nextval('app.standalone')",
];

#[tokio::test(flavor = "multi_thread")]
async fn postgres_integrated_backup_restores_into_an_empty_database() {
    if !enabled() {
        return;
    }
    let (cfg, s) = connect(DbKind::Postgres).await;
    let n = unique();
    let (src, dst) = (format!("bk_it_src_{n}"), format!("bk_it_dst_{n}"));
    exec(&*s, None, &format!("CREATE DATABASE {src}")).await;
    for sql in PG_FIXTURE {
        exec(&*s, Some(&src), sql).await;
    }
    let dir = tempfile::tempdir().unwrap();
    let rep = Arc::new(Rep::default());
    let pass = Secret::new("correct horse");
    let out = run_backup(
        ctx(&cfg, &s, &rep, BTreeMap::new()),
        &spec(&src, true, dir.path(), logical()),
        Some(&pass),
    )
    .await
    .unwrap();
    assert_eq!(out.files.len(), 1);
    assert!(out.rows >= 5303, "rows {}", out.rows);
    let file = PathBuf::from(&out.files[0]);
    assert!(rep.phases.lock().unwrap().iter().any(|p| p == "data"));

    // The file is encrypted: no preamble without the passphrase, integrity with a wrong one.
    let ins = inspect(&file, None).await.unwrap();
    assert!(ins.encrypted && ins.preamble.is_none());
    assert_eq!(
        inspect(&file, Some(&Secret::new("wrong")))
            .await
            .unwrap_err()
            .kind(),
        "integrity"
    );
    let ins = inspect(&file, Some(&pass)).await.unwrap();
    assert_eq!(ins.preamble.as_ref().unwrap()["database"], src.as_str());
    verify(&file, Some(&pass), &*rep).await.unwrap();

    // Wrong passphrase must fail before the target is touched.
    let mut rspec = RestoreSpec {
        file: file.to_string_lossy().into_owned(),
        target_connection_id: Uuid::nil(),
        target_database: Some(dst.clone()),
        create_database: true,
        clean: false,
        objects: vec![],
        continue_on_error: false,
    };
    let err = run_restore(
        ctx(&cfg, &s, &rep, BTreeMap::new()),
        &rspec,
        Some(&Secret::new("bad")),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), "integrity");
    let exists = execute_collect(
        &*s,
        QueryRequest::new(format!("SELECT 1 FROM pg_database WHERE datname = '{dst}'")),
    )
    .await
    .unwrap();
    assert!(
        matches!(&exists[0].outcome, dbstudio_core::StatementOutcome::Rows(r) if r.rows.is_empty())
    );

    let res = run_restore(ctx(&cfg, &s, &rep, BTreeMap::new()), &rspec, Some(&pass))
        .await
        .unwrap();
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    for t in ["customers", "orders"] {
        let a = rows_of(&*s, pg_table(&src, "app", t)).await;
        let b = rows_of(&*s, pg_table(&dst, "app", t)).await;
        assert!(!a.is_empty());
        assert_eq!(a, b, "table {t}");
    }
    // View, sequence state and serial continuation survive.
    let probe = |sql: &'static str, db: &str| {
        let (s, db) = (s.clone(), db.to_string());
        async move {
            let mut req = QueryRequest::new(sql);
            req.database = Some(db);
            let r = execute_collect(&*s, req).await;
            match r.unwrap_or_else(|e| panic!("{sql}: {e}")).remove(0).outcome {
                dbstudio_core::StatementOutcome::Rows(r) => r.rows[0][0].clone(),
                o => panic!("{o:?}"),
            }
        }
    };
    for sql in [
        "SELECT count(*) FROM app.big_orders",
        "SELECT nextval('app.standalone')",
        "SELECT nextval('app.customers_id_seq')",
        "SELECT nextval('app.orders_id_seq')",
        "SELECT count(*) FROM pg_indexes WHERE indexname = 'orders_customer'",
    ] {
        assert_eq!(probe(sql, &src).await, probe(sql, &dst).await, "{sql}");
    }

    // A second restore into the (now populated) database fails; `clean` replaces the content.
    assert!(
        run_restore(ctx(&cfg, &s, &rep, BTreeMap::new()), &rspec, Some(&pass))
            .await
            .is_err()
    );
    rspec.create_database = false;
    rspec.clean = true;
    let again = run_restore(ctx(&cfg, &s, &rep, BTreeMap::new()), &rspec, Some(&pass))
        .await
        .unwrap();
    assert!(again.errors.is_empty(), "{:?}", again.errors);
    assert_eq!(
        rows_of(&*s, pg_table(&src, "app", "orders")).await,
        rows_of(&*s, pg_table(&dst, "app", "orders")).await
    );

    // Subset restore: only the customers table (and its data) into another empty database.
    let sub = format!("bk_it_sub_{n}");
    rspec.target_database = Some(sub.clone());
    rspec.create_database = true;
    rspec.clean = false;
    rspec.objects = vec!["app.customers".into()];
    let r = run_restore(ctx(&cfg, &s, &rep, BTreeMap::new()), &rspec, Some(&pass))
        .await
        .unwrap();
    assert_eq!(r.objects, 1);
    assert_eq!(
        rows_of(&*s, pg_table(&src, "app", "customers")).await,
        rows_of(&*s, pg_table(&sub, "app", "customers")).await
    );
    for db in [&src, &dst, &sub] {
        exec(
            &*s,
            None,
            &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
        )
        .await;
    }
}

const MY_FIXTURE: &[&str] = &[
    "CREATE TABLE customers (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100) NOT NULL, \
     email VARCHAR(100) UNIQUE, created DATETIME DEFAULT '2026-01-02 03:04:05', meta JSON, \
     avatar BLOB, balance DECIMAL(12,2), active TINYINT(1), \
     full_name VARCHAR(120) GENERATED ALWAYS AS (CONCAT(name, '!')) STORED)",
    "CREATE TABLE orders (id BIGINT AUTO_INCREMENT PRIMARY KEY, customer_id INT, total DOUBLE, \
     note TEXT, KEY orders_customer (customer_id), \
     CONSTRAINT fk_cust FOREIGN KEY (customer_id) REFERENCES customers(id))",
    "CREATE VIEW big_orders AS SELECT id, total FROM orders WHERE total > 100",
    "INSERT INTO customers (name, email, meta, avatar, balance, active) VALUES \
     ('Ann ''quoted''; \"dq\" \\\\ back', 'a@x', '{\"k\": [1, 2]}', X'DEADBEEF', 12.50, 1), \
     ('Bé Ünïcode 日本', NULL, NULL, NULL, NULL, 0), ('Line\\nbreak', 'c@x', 'null', X'', -0.01, NULL)",
    "INSERT INTO orders (customer_id, total, note) \
     SELECT (n % 3) + 1, n * 1.5, CONCAT('n', n) FROM (SELECT ROW_NUMBER() OVER () AS n \
     FROM information_schema.columns a, information_schema.columns b LIMIT 5300) t",
];

#[tokio::test(flavor = "multi_thread")]
async fn mysql_integrated_backup_restores_into_an_empty_database() {
    if !enabled() {
        return;
    }
    let (cfg, s) = connect(DbKind::MySql).await;
    let n = unique();
    let (src, dst) = (
        format!("dbstudio_it_bk_src_{n}"),
        format!("dbstudio_it_bk_dst_{n}"),
    );
    exec(&*s, None, &format!("CREATE DATABASE `{src}`")).await;
    for sql in MY_FIXTURE {
        exec(&*s, Some(&src), sql).await;
    }
    let dir = tempfile::tempdir().unwrap();
    let rep = Arc::new(Rep::default());
    let mut sp = spec(&src, false, dir.path(), logical());
    sp.compression = Compression::Gzip { level: 6 };
    let out = run_backup(ctx(&cfg, &s, &rep, BTreeMap::new()), &sp, None)
        .await
        .unwrap();
    let file = PathBuf::from(&out.files[0]);
    if let Ok(keep) = std::env::var("DBSTUDIO_KEEP") {
        std::fs::copy(&file, keep).unwrap();
    }
    verify(&file, None, &*rep).await.unwrap();
    let rspec = RestoreSpec {
        file: file.to_string_lossy().into_owned(),
        target_connection_id: Uuid::nil(),
        target_database: Some(dst.clone()),
        create_database: true,
        clean: false,
        objects: vec![],
        continue_on_error: false,
    };
    let res = run_restore(ctx(&cfg, &s, &rep, BTreeMap::new()), &rspec, None)
        .await
        .unwrap();
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    for t in ["customers", "orders"] {
        let a = rows_of(&*s, my_table(&src, t)).await;
        let b = rows_of(&*s, my_table(&dst, t)).await;
        assert!(!a.is_empty());
        assert_eq!(a, b, "table {t}");
    }
    let count = |db: String| {
        let s = s.clone();
        async move {
            let mut req = QueryRequest::new("SELECT COUNT(*) FROM big_orders");
            req.database = Some(db);
            match execute_collect(&*s, req).await.unwrap().remove(0).outcome {
                dbstudio_core::StatementOutcome::Rows(r) => r.rows[0][0].clone(),
                o => panic!("{o:?}"),
            }
        }
    };
    assert_eq!(count(src.clone()).await, count(dst.clone()).await);
    for db in [&src, &dst] {
        exec(&*s, None, &format!("DROP DATABASE IF EXISTS `{db}`")).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Native tools through docker exec shims
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
fn write_shims(dir: &Path) -> BTreeMap<NativeTool, String> {
    use std::os::unix::fs::PermissionsExt;
    let pg = r#"#!/bin/bash
args=(); while [ $# -gt 0 ]; do case "$1" in -h|-p) shift 2;; *) args+=("$1"); shift;; esac; done
exec docker exec -i -e PGPASSWORD -e PGSSLMODE dbstudio-test-pg16-1 TOOL "${args[@]}"
"#;
    let my = r#"#!/bin/bash
args=(); pw=""
while [ $# -gt 0 ]; do case "$1" in
  --defaults-extra-file=*) f="${1#--defaults-extra-file=}"; pw=$(sed -n 's/^password="\(.*\)"$/\1/p' "$f"); shift;;
  -h|-P) shift 2;; --ssl-mode=*) shift;; *) args+=("$1"); shift;; esac; done
exec docker exec -i -e MYSQL_PWD="$pw" dbstudio-test-mysql8-1 TOOL "${args[@]}"
"#;
    let mut map = BTreeMap::new();
    for (tool, name, tpl) in [
        (NativeTool::PgDump, "pg_dump", pg),
        (NativeTool::PgRestore, "pg_restore", pg),
        (NativeTool::Psql, "psql", pg),
        (NativeTool::MySqlDump, "mysqldump", my),
        (NativeTool::MySqlClient, "mysql", my),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, tpl.replace("TOOL", name)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        map.insert(tool, path.to_string_lossy().into_owned());
    }
    map
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn native_pg_dump_and_mysqldump_roundtrip_through_shims() {
    if !enabled() {
        return;
    }
    let shims = tempfile::tempdir().unwrap();
    let tools = write_shims(shims.path());
    let rep = Arc::new(Rep::default());
    let n = unique();

    // PostgreSQL: pg_dump -Fc -> container (encrypted) -> pg_restore into an empty database.
    let (cfg, s) = connect(DbKind::Postgres).await;
    let (src, dst) = (format!("bk_it_nsrc_{n}"), format!("bk_it_ndst_{n}"));
    exec(&*s, None, &format!("CREATE DATABASE {src}")).await;
    for sql in &PG_FIXTURE[..6] {
        exec(&*s, Some(&src), sql).await;
    }
    exec(
        &*s,
        Some(&src),
        "INSERT INTO app.customers (name, balance) VALUES ('x', 1.5), ('y', 2.5)",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let pass = Secret::new("pw");
    let engine = BackupEngine::Native {
        tool: NativeTool::PgDump,
        options: BTreeMap::new(),
    };
    let out = run_backup(
        ctx(&cfg, &s, &rep, tools.clone()),
        &spec(&src, true, dir.path(), engine),
        Some(&pass),
    )
    .await
    .unwrap();
    let file = PathBuf::from(&out.files[0]);
    let ins = inspect(&file, Some(&pass)).await.unwrap();
    assert_eq!(ins.native, Some(NativeTool::PgDump));
    let rspec = RestoreSpec {
        file: file.to_string_lossy().into_owned(),
        target_connection_id: Uuid::nil(),
        target_database: Some(dst.clone()),
        create_database: true,
        clean: false,
        objects: vec![],
        continue_on_error: false,
    };
    run_restore(ctx(&cfg, &s, &rep, tools.clone()), &rspec, Some(&pass))
        .await
        .unwrap();
    assert_eq!(
        rows_of(&*s, pg_table(&src, "app", "customers")).await,
        rows_of(&*s, pg_table(&dst, "app", "customers")).await
    );
    for db in [&src, &dst] {
        exec(
            &*s,
            None,
            &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
        )
        .await;
    }

    // MySQL: mysqldump -> container -> mysql into an empty database.
    let (cfg, s) = connect(DbKind::MySql).await;
    let (src, dst) = (
        format!("dbstudio_it_nsrc_{n}"),
        format!("dbstudio_it_ndst_{n}"),
    );
    exec(&*s, None, &format!("CREATE DATABASE `{src}`")).await;
    for sql in &MY_FIXTURE[..3] {
        exec(&*s, Some(&src), sql).await;
    }
    exec(
        &*s,
        Some(&src),
        "INSERT INTO customers (name, balance) VALUES ('x', 1.5), ('y', 2.5)",
    )
    .await;
    let engine = BackupEngine::Native {
        tool: NativeTool::MySqlDump,
        options: BTreeMap::new(),
    };
    let dir = tempfile::tempdir().unwrap();
    let out = run_backup(
        ctx(&cfg, &s, &rep, tools.clone()),
        &spec(&src, false, dir.path(), engine),
        None,
    )
    .await
    .unwrap();
    let rspec = RestoreSpec {
        file: out.files[0].clone(),
        target_connection_id: Uuid::nil(),
        target_database: Some(dst.clone()),
        create_database: true,
        clean: false,
        objects: vec![],
        continue_on_error: false,
    };
    run_restore(ctx(&cfg, &s, &rep, tools), &rspec, None)
        .await
        .unwrap();
    assert_eq!(
        rows_of(&*s, my_table(&src, "customers")).await,
        rows_of(&*s, my_table(&dst, "customers")).await
    );
    for db in [&src, &dst] {
        exec(&*s, None, &format!("DROP DATABASE IF EXISTS `{db}`")).await;
    }
}

/// With no `pg_dump` anywhere the run fails with the tool's problem (nothing is written).
#[tokio::test(flavor = "multi_thread")]
async fn native_backup_without_the_tool_reports_the_problem() {
    if !enabled() {
        return;
    }
    let found = dbstudio_backup::detect_tools(&BTreeMap::new())
        .into_iter()
        .any(|t| t.tool == NativeTool::PgDump && t.path.is_some());
    if found {
        eprintln!("pg_dump exists on this host; the absent-tool path is covered by unit tests");
        return;
    }
    let (cfg, s) = connect(DbKind::Postgres).await;
    let dir = tempfile::tempdir().unwrap();
    let engine = BackupEngine::Native {
        tool: NativeTool::PgDump,
        options: BTreeMap::new(),
    };
    let rep = Arc::new(Rep::default());
    let err = run_backup(
        ctx(&cfg, &s, &rep, BTreeMap::new()),
        &spec("dbstudio", false, dir.path(), engine),
        None,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("pg_dump"), "{err}");
    assert!(err.to_string().contains("not found"), "{err}");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}
