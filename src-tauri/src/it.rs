//! End-to-end tests of the command layer against real servers (PostgreSQL 16 and MySQL 8.4 from
//! `docker/compose.test.yml`). They run only when `DBSTUDIO_IT=1`; otherwise they return at once.
//!
//! Everything goes through the plain `*_impl(&AppState, ..)` functions behind the IPC commands,
//! with an in-memory database, an in-memory secret store and a closure instead of a Tauri
//! channel. Object paths are never built by hand: the tree is walked by reusing the `path` of
//! the nodes the driver returns, as the UI does.

use std::path::PathBuf;
use std::time::Duration;

use dbstudio_core::{
    ConnectionConfig, DbKind, DriverStatus, Error, ObjectKind, ObjectNode, ObjectPath, QueryEvent,
    QueryRequest, Secret, TableCountRequest, TableDataRequest, TlsConfig, TlsMode, execute_collect,
};
use dbstudio_store::{ConnectionProfile, SecretChange};
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::commands::{browse, connections, sessions};
use crate::state::testing::test_state;
use crate::state::{AppState, build_registry};

const PASSWORD: &str = "dbstudio_test";
const TABLE: &str = "it_items";

fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").as_deref() == Ok("1")
}

fn cert(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../docker/certs")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// Per-server parameters.
struct Target {
    kind: DbKind,
    port: u16,
    product: &'static str,
    /// Names of containers (database / schema) the tree walk may descend into.
    containers: &'static [&'static str],
    /// Statements that (re)create the fixture table.
    setup: &'static [&'static str],
    /// `raw_where` that blocks for a long time.
    slow_where: &'static str,
    /// Whether the server keeps JSON text verbatim (PostgreSQL `json`; MySQL normalizes).
    json_verbatim: bool,
}

const JSON_DOC: &str = r#"{"b":1,"a":2,"i":9007199254740993}"#;

fn profile(t: &Target, save_password: bool, tls: TlsMode, ca: Option<&str>) -> ConnectionProfile {
    let mut config = ConnectionConfig::new(t.kind, "it", "127.0.0.1");
    config.port = t.port;
    config.database = Some("dbstudio".into());
    config.username = Some("dbstudio".into());
    config.connect_timeout_ms = Some(5000);
    config.tls = TlsConfig {
        mode: tls,
        ca_cert_path: ca.map(str::to_string),
        ..TlsConfig::default()
    };
    ConnectionProfile {
        config,
        group_id: None,
        color: None,
        save_password,
        has_password: false,
        sort_order: 0,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn set(v: &str) -> SecretChange {
    SecretChange::Set {
        value: Secret::new(v),
    }
}

fn kind_of(e: &Error) -> String {
    let wire = serde_json::to_value(e).unwrap();
    // Messages no longer start with an English error-category prefix.
    let message = wire["message"].as_str().unwrap();
    for prefix in [
        "authentication failed",
        "tls error",
        "not found",
        "connection error",
        "storage error",
    ] {
        assert!(!message.starts_with(prefix), "prefixed message: {message}");
    }
    wire["kind"].as_str().unwrap().to_string()
}

/// With `DBSTUDIO_IT=1` an unreachable server is a failure, not a silent skip.
fn require_reachable(port: u16) {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    if let Err(e) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
        panic!("DBSTUDIO_IT=1 but nothing listens on {addr} ({e}); start docker/compose.test.yml");
    }
}

/// Walk the tree from the root, only ever using paths returned by the driver, until the fixture
/// table is found.
async fn find_table(state: &AppState, sid: Uuid, t: &Target) -> ObjectNode {
    let mut stack = vec![ObjectPath::root()];
    while let Some(path) = stack.pop() {
        let nodes = sessions::children_impl(state, sid, path.clone())
            .await
            .unwrap_or_else(|e| panic!("object_children failed: {e}"));
        for node in nodes {
            // The node's path must extend its parent's path.
            assert!(node.path.segments.starts_with(&path.segments));
            if node.kind == ObjectKind::Table && node.name == TABLE {
                return node;
            }
            let descend = node.has_children
                && match node.kind {
                    ObjectKind::Folder => ["schemas", "tables"].contains(&node.name.as_str()),
                    ObjectKind::Database | ObjectKind::Schema => {
                        t.containers.contains(&node.name.as_str())
                    }
                    _ => false,
                };
            if descend {
                stack.push(node.path.clone());
            }
        }
    }
    panic!("table {TABLE} not found in the object tree");
}

async fn run_sql(state: &AppState, sid: Uuid, sql: &str) {
    let session = state.sessions.get(sid).unwrap();
    execute_collect(&*session, QueryRequest::new(sql))
        .await
        .unwrap_or_else(|e| panic!("sql failed: {e}\n{sql}"));
}

fn fetch_request(table: &ObjectPath) -> TableDataRequest {
    TableDataRequest {
        query_id: Uuid::new_v4(),
        table: table.clone(),
        filters: vec![],
        raw_where: None,
        sort: vec![dbstudio_core::SortKey {
            column: "id".into(),
            descending: false,
        }],
        offset: 0,
        limit: 100,
        batch_size: Some(2),
    }
}

async fn scenario(t: &Target) {
    require_reachable(t.port);
    let (state, secrets) = test_state(build_registry());

    // 1. connection_test with an unsaved profile.
    let tls = profile(t, true, TlsMode::Require, None);
    let info = connections::test_impl(&state, tls.clone(), set(PASSWORD))
        .await
        .expect("connection_test");
    assert!(
        info.product.to_lowercase().contains(t.product),
        "product {:?}",
        info.product
    );
    assert!(!info.version.is_empty());
    let err = connections::test_impl(&state, tls, set("wrong-password"))
        .await
        .unwrap_err();
    assert_eq!(kind_of(&err), "authentication", "{err}");
    let verified = profile(t, true, TlsMode::VerifyFull, Some(&cert("ca.crt")));
    connections::test_impl(&state, verified, set(PASSWORD))
        .await
        .expect("verify-full with the right CA");
    let wrong_ca = profile(t, true, TlsMode::VerifyFull, Some(&cert("other-ca.crt")));
    let err = connections::test_impl(&state, wrong_ca, set(PASSWORD))
        .await
        .unwrap_err();
    assert_eq!(kind_of(&err), "tls", "{err}");
    assert!(
        connections::list_impl(&state)
            .await
            .unwrap()
            .profiles
            .is_empty()
    );
    assert!(state.sessions.is_empty());

    // 2. Save with password, open with the stored one.
    let saved = connections::save_impl(
        &state,
        profile(t, true, TlsMode::VerifyFull, Some(&cert("ca.crt"))),
        set(PASSWORD),
    )
    .await
    .unwrap();
    assert!(saved.has_password);
    assert_eq!(secrets.len(), 1);
    let opened = sessions::open_impl(&state, saved.config.id, None)
        .await
        .expect("session_open with stored password");
    assert_eq!(opened.descriptor.status, DriverStatus::Available);
    assert_eq!(opened.descriptor.kind, t.kind);
    assert_eq!(opened.connection_id, saved.config.id);
    let sid = opened.session_id;

    // 3. A profile that does not store its password.
    let ask = connections::save_impl(
        &state,
        profile(t, false, TlsMode::Prefer, None),
        SecretChange::Keep,
    )
    .await
    .unwrap();
    let err = sessions::open_impl(&state, ask.config.id, None)
        .await
        .unwrap_err();
    assert_eq!(kind_of(&err), "authentication", "{err}");
    let typed = sessions::open_impl(&state, ask.config.id, Some(Secret::new(PASSWORD)))
        .await
        .expect("session_open with supplied password");
    sessions::close_impl(&state, typed.session_id)
        .await
        .unwrap();
    assert_eq!(secrets.len(), 1, "the typed password is not persisted");

    // 4. Fixture table, then walk the tree by reusing returned paths.
    for sql in t.setup {
        run_sql(&state, sid, sql).await;
    }
    let node = find_table(&state, sid, t).await;
    assert_eq!(node.kind, ObjectKind::Table);
    let path = node.path.clone();
    // Nodes round-trip through JSON exactly as the frontend sends paths back.
    let echoed: ObjectPath = serde_json::from_value(serde_json::to_value(&path).unwrap()).unwrap();
    assert_eq!(echoed, path);

    // 5. details, ddl, count, fetch.
    let details = browse::details_impl(&state, sid, path.clone())
        .await
        .unwrap();
    let names: Vec<_> = details.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "amount", "note", "doc"]);
    assert!(details.columns[0].primary_key && !details.columns[1].primary_key);
    assert!(details.columns[1].nullable);
    let ddl = sessions::ddl_impl(&state, sid, path.clone()).await.unwrap();
    assert!(ddl.to_lowercase().contains(TABLE), "{ddl}");
    let count = browse::count_impl(
        &state,
        sid,
        TableCountRequest {
            table: path.clone(),
            filters: vec![],
            raw_where: None,
            query_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(count, 3);

    let mut events: Vec<QueryEvent> = Vec::new();
    browse::fetch_impl(&state, sid, fetch_request(&path), |e| {
        events.push(e);
        true
    })
    .await
    .expect("table_fetch");
    let wire: Vec<Json> = events
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    assert_eq!(wire.first().unwrap()["type"], "columns");
    assert_eq!(wire.last().unwrap()["type"], "statementEnd");
    assert!(wire[1..wire.len() - 1].iter().all(|e| e["type"] == "rows"));
    let cols = wire[0]["columns"].as_array().unwrap();
    assert_eq!(cols.len(), 4);
    for key in ["name", "typeName", "valueKind", "nullable"] {
        assert!(cols[0].get(key).is_some(), "column meta lacks {key}");
    }
    assert_eq!(cols[0]["name"], "id");
    let rows: Vec<&Json> = wire[1..wire.len() - 1]
        .iter()
        .flat_map(|e| e["rows"].as_array().unwrap())
        .collect();
    assert_eq!(rows.len(), 3);
    assert!(wire.len() > 3, "batch_size 2 yields several row events");
    assert_eq!(rows[0][0], json!({"t": "int", "v": 1}));
    assert_eq!(rows[0][1], json!({"t": "null"}));
    assert_eq!(rows[0][2], json!({"t": "null"}));
    assert_eq!(rows[1][1], json!({"t": "decimal", "v": "0.5000"}));
    assert_eq!(rows[1][2], json!({"t": "text", "v": "c"}));
    // JSON arrives as the raw text: digits beyond 2^53 survive, and key order does where the
    // server keeps it.
    assert_eq!(rows[0][3], json!({"t": "null"}));
    assert_eq!(rows[1][3]["t"], "json");
    let raw = rows[1][3]["v"].as_str().expect("json cell is a string");
    assert!(raw.contains("9007199254740993"), "{raw}");
    if t.json_verbatim {
        assert_eq!(raw, JSON_DOC);
        assert!(raw.find("\"b\"").unwrap() < raw.find("\"a\"").unwrap());
    }
    // Beyond 2^53 the integer travels as a string; so does a big decimal.
    assert_eq!(rows[2][0], json!({"t": "int", "v": "9007199254740993"}));
    assert_eq!(
        rows[2][1],
        json!({"t": "decimal", "v": "12345678901234.5678"})
    );
    let end = &wire.last().unwrap()["end"];
    assert_eq!(end["outcome"]["kind"], "rows");
    assert_eq!(end["outcome"]["rowCount"], 3);
    assert_eq!(end["outcome"]["truncated"], false);
    assert!(end["elapsedMs"].is_number());

    // 6. Cancel a slow fetch from another task.
    let mut slow = fetch_request(&path);
    slow.raw_where = Some(t.slow_where.into());
    let qid = slow.query_id;
    let started = std::time::Instant::now();
    let (result, cancelled) =
        tokio::join!(browse::fetch_impl(&state, sid, slow, |_| true), async {
            tokio::time::sleep(Duration::from_millis(600)).await;
            sessions::cancel_impl(&state, sid, qid).await
        });
    cancelled.expect("query_cancel");
    let err = result.expect_err("fetch must end with an error");
    assert_eq!(kind_of(&err), "cancelled", "{err}");
    assert!(started.elapsed() < Duration::from_secs(20));
    // The session is still usable afterwards.
    assert_eq!(
        browse::count_impl(
            &state,
            sid,
            TableCountRequest {
                table: path.clone(),
                filters: vec![],
                raw_where: None,
                query_id: None,
            }
        )
        .await
        .unwrap(),
        3
    );

    // 6b. A slow table_count is cancellable through its query id too.
    let count_qid = Uuid::new_v4();
    let slow_count = TableCountRequest {
        table: path.clone(),
        filters: vec![],
        raw_where: Some(t.slow_where.into()),
        query_id: Some(count_qid),
    };
    let (result, cancelled) = tokio::join!(browse::count_impl(&state, sid, slow_count), async {
        tokio::time::sleep(Duration::from_millis(600)).await;
        sessions::cancel_impl(&state, sid, count_qid).await
    });
    cancelled.expect("query_cancel for table_count");
    let err = result.expect_err("count must end with an error");
    assert_eq!(kind_of(&err), "cancelled", "{err}");

    // 7. Deleting the connection closes its session.
    connections::delete_impl(&state, saved.config.id)
        .await
        .unwrap();
    assert!(secrets.is_empty());
    let err = sessions::children_impl(&state, sid, ObjectPath::root())
        .await
        .unwrap_err();
    assert_eq!(kind_of(&err), "notFound", "{err}");
    assert!(state.sessions.is_empty());
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_end_to_end() {
    if !enabled() {
        return;
    }
    scenario(&Target {
        kind: DbKind::Postgres,
        port: 54316,
        product: "postgres",
        containers: &["dbstudio", "dbstudio_it"],
        setup: &[
            "DROP SCHEMA IF EXISTS dbstudio_it CASCADE",
            "CREATE SCHEMA dbstudio_it",
            "CREATE TABLE dbstudio_it.it_items (id bigint PRIMARY KEY, amount numeric(20,4), note text, doc json)",
            "INSERT INTO dbstudio_it.it_items VALUES \
             (9007199254740993, 12345678901234.5678, 'big', NULL), (1, NULL, NULL, NULL), \
             (2, 0.5, 'c', '{\"b\":1,\"a\":2,\"i\":9007199254740993}')",
        ],
        slow_where: "pg_sleep(60) IS NOT NULL",
        json_verbatim: true,
    })
    .await;
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql_end_to_end() {
    if !enabled() {
        return;
    }
    scenario(&Target {
        kind: DbKind::MySql,
        port: 33068,
        product: "mysql",
        containers: &["dbstudio"],
        setup: &[
            "DROP TABLE IF EXISTS it_items",
            "CREATE TABLE it_items (id BIGINT PRIMARY KEY, amount DECIMAL(20,4), note VARCHAR(50), doc JSON)",
            "INSERT INTO it_items VALUES \
             (9007199254740993, 12345678901234.5678, 'big', NULL), (1, NULL, NULL, NULL), \
             (2, 0.5, 'c', '{\"b\":1,\"a\":2,\"i\":9007199254740993}')",
        ],
        slow_where: "SLEEP(60) = 0",
        json_verbatim: false,
    })
    .await;
}

/// MongoDB over SSH (`sshd` + `mongo7` services): one SOCKS5 proxy on the SSH session keeps the
/// profile's original host, so `verifyFull` checks the certificate against `mongo7`, and a
/// replica set (seed list `hosts`, no `directConnection`) reaches its member by its own name.
#[cfg(feature = "mongodb")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mongodb_over_ssh_verifies_the_original_host_and_reaches_the_replica_set() {
    use std::sync::Arc;

    use dbstudio_core::{DriverRegistry, SshAuth, SshConfig};
    use dbstudio_ssh::MemoryKnownHosts;

    use crate::session::{SessionManager, SshEnv};

    if !enabled() {
        return;
    }
    let ssh_port: u16 = std::env::var("DBSTUDIO_IT_SSH_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(22022);
    let known = Arc::new(MemoryKnownHosts::new());
    let env = SshEnv {
        known: known.clone(),
        secret: Some(Secret::new("dbstudio_ssh")),
    };
    let mut registry = DriverRegistry::new();
    registry.register(Arc::new(dbstudio_driver_mongodb::MongoDriver::new()));
    let manager = SessionManager::new(Arc::new(registry));

    let base = |direct: bool| {
        let mut c = ConnectionConfig::new(DbKind::MongoDb, "it", "mongo7");
        c.port = 27017;
        c.username = Some("dbstudio".into());
        c.tls = TlsConfig {
            mode: TlsMode::VerifyFull,
            ca_cert_path: Some(cert("ca.crt")),
            ..TlsConfig::default()
        };
        c.options
            .insert("authSource".to_string(), "admin".to_string());
        if direct {
            c.options
                .insert("directConnection".to_string(), "true".to_string());
        } else {
            c.options
                .insert("hosts".to_string(), "mongo7:27017".to_string());
            c.options
                .insert("replicaSet".to_string(), "rs0".to_string());
        }
        c.ssh = Some(SshConfig {
            host: "127.0.0.1".into(),
            port: ssh_port,
            username: "dbstudio".into(),
            auth: SshAuth::Password,
            keepalive_secs: Some(15),
            connect_timeout_ms: Some(15_000),
        });
        c
    };
    let password = Secret::new("dbstudio_test");

    for direct in [true, false] {
        let config = base(direct);
        // The first connection stops at the unknown SSH host key; trust it and retry.
        let mut attempt = manager
            .open_with_ssh(&config, Some(&password), Some(&env))
            .await;
        if let Err(Error::SshHostKey {
            host,
            port,
            key_type,
            fingerprint,
            ..
        }) = &attempt
        {
            known.trust(host, *port, key_type, fingerprint);
            attempt = manager
                .open_with_ssh(&config, Some(&password), Some(&env))
                .await;
        }
        let info = attempt.unwrap_or_else(|e| panic!("direct={direct}: {e}"));
        let session = manager.get(info.session_id).unwrap();
        let mut req = QueryRequest::new("db.runCommand({ping: 1})");
        req.database = Some("admin".into());
        let results = execute_collect(session.as_ref(), req)
            .await
            .unwrap_or_else(|e| panic!("ping direct={direct}: {e}"));
        assert!(!results.is_empty());
        manager.close(info.session_id).await.unwrap();
    }
}
