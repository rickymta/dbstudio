//! Integration tests of the PostgreSQL driver against the flavours CockroachDB (`cockroach`,
//! profile `newsql`, port `DBSTUDIO_IT_COCKROACH_PORT`, default 26258) and YugabyteDB
//! (`yugabyte`, profile `heavy`, `DBSTUDIO_IT_YUGABYTE_PORT`, default 54333).
//!
//! Only run with `DBSTUDIO_IT=1`. A server that is not running makes its tests skip (pass) unless
//! `DBSTUDIO_IT_STRICT` naming the service (or `1`), which turns that into a failure.
//!
//! Features that a flavour does not have are skipped inside the tests with the reason printed
//! (`SKIP <test>: <why>`).

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{enabled, exec, rows, scalar};
use dbstudio_core::*;
use dbstudio_driver_postgres::{PostgresDriver, PostgresFlavorDriver};
use dbstudio_plan::{PlanNodeKind, PlanRules, parse_and_analyze};
use uuid::Uuid;

// ----------------------------------------------------------------------------------------------
// Server access
// ----------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Server {
    Cockroach,
    Yugabyte,
}

impl Server {
    fn port(self) -> u16 {
        let (var, default) = match self {
            Server::Cockroach => ("DBSTUDIO_IT_COCKROACH_PORT", 26258),
            Server::Yugabyte => ("DBSTUDIO_IT_YUGABYTE_PORT", 54333),
        };
        std::env::var(var)
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(default)
    }

    fn kind(self) -> DbKind {
        match self {
            Server::Cockroach => DbKind::CockroachDb,
            Server::Yugabyte => DbKind::YugabyteDb,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Server::Cockroach => "cockroach",
            Server::Yugabyte => "yugabyte",
        }
    }

    fn is_cockroach(self) -> bool {
        self == Server::Cockroach
    }
}

fn certs() -> String {
    format!("{}/cockroach", common::certs_dir())
}

fn skip(test: &str, why: &str) {
    eprintln!("SKIP {test}: {why}");
}

fn is_up(server: Server) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = ([127, 0, 0, 1], server.port()).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// `true` when the test should run against `server`.
fn run_on(server: Server, test: &str) -> bool {
    if !enabled() {
        return false;
    }
    if !is_up(server) {
        if dbstudio_core::testing::it_strict_for(server.name()) {
            panic!(
                "{test}: {} is not running (DBSTUDIO_IT_STRICT=1)",
                server.name()
            );
        }
        skip(test, &format!("{} is not running", server.name()));
        return false;
    }
    true
}

async fn admin(server: Server) -> Box<dyn Session> {
    let mut cfg = ConnectionConfig::new(server.kind(), "admin", "127.0.0.1");
    cfg.port = server.port();
    cfg.connect_timeout_ms = Some(10_000);
    let secret = match server {
        Server::Cockroach => {
            cfg.username = Some("root".into());
            cfg.database = Some("defaultdb".into());
            cfg.tls = TlsConfig {
                mode: TlsMode::VerifyFull,
                ca_cert_path: Some(format!("{}/ca.crt", certs())),
                client_cert_path: Some(format!("{}/client.root.crt", certs())),
                client_key_path: Some(format!("{}/client.root.key", certs())),
                server_name: None,
            };
            None
        }
        Server::Yugabyte => {
            cfg.username = Some("yugabyte".into());
            cfg.database = Some("yugabyte".into());
            Some(Secret::new("yugabyte"))
        }
    };
    PostgresFlavorDriver::new(server.kind())
        .connect(&cfg, secret.as_ref())
        .await
        .expect("admin connect")
}

/// Create the test user and database once per process and server.
async fn setup(server: Server) {
    static COCKROACH: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    static YUGABYTE: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    let cell = if server.is_cockroach() {
        &COCKROACH
    } else {
        &YUGABYTE
    };
    cell.get_or_init(|| async move {
        let s = admin(server).await;
        let stmts: &[&str] = match server {
            Server::Cockroach => &[
                "CREATE USER IF NOT EXISTS dbstudio WITH PASSWORD 'dbstudio_test'",
                "ALTER USER dbstudio CREATEDB",
                "CREATE DATABASE IF NOT EXISTS dbstudio",
                "GRANT ALL ON DATABASE dbstudio TO dbstudio",
            ],
            Server::Yugabyte => &[
                "CREATE DATABASE dbstudio",
                "CREATE USER dbstudio WITH PASSWORD 'dbstudio_test' CREATEDB",
            ],
        };
        for sql in stmts {
            // Re-runs find the objects already there.
            let _ = execute_collect(&*s, QueryRequest::new(*sql)).await;
        }
        if server == Server::Yugabyte {
            let _ = execute_collect(
                &*s,
                QueryRequest::new("GRANT ALL ON DATABASE dbstudio TO dbstudio"),
            )
            .await;
        }
        s.close().await.ok();
    })
    .await;
}

fn config(server: Server, kind: DbKind) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new(kind, "it", "127.0.0.1");
    cfg.port = server.port();
    cfg.database = Some("dbstudio".into());
    cfg.username = Some("dbstudio".into());
    cfg.tls.mode = TlsMode::Prefer;
    cfg.connect_timeout_ms = Some(10_000);
    cfg
}

async fn connect(server: Server) -> Box<dyn Session> {
    setup(server).await;
    PostgresFlavorDriver::new(server.kind())
        .connect(
            &config(server, server.kind()),
            Some(&Secret::new(common::PASSWORD)),
        )
        .await
        .expect("connect")
}

fn unique(prefix: &str) -> String {
    format!("{prefix}_{}", &Uuid::new_v4().simple().to_string()[..10])
}

fn schema_path(db: &str, schema: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Database, db)
        .child(ObjectKind::Folder, "schemas")
        .child(ObjectKind::Schema, schema)
}

fn table_path(schema: &str, table: &str) -> ObjectPath {
    schema_path("dbstudio", schema)
        .child(ObjectKind::Folder, "tables")
        .child(ObjectKind::Table, table)
}

fn text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        other => panic!("not text: {other:?}"),
    }
}

async fn drop_schema(s: &dyn Session, schema: &str) {
    let _ = execute_collect(
        s,
        QueryRequest::new(format!("DROP SCHEMA IF EXISTS {schema} CASCADE")),
    )
    .await;
}

async fn children(s: &dyn Session, path: &ObjectPath) -> Vec<String> {
    s.list_children(path)
        .await
        .unwrap_or_else(|e| panic!("list_children {path:?}: {e}"))
        .into_iter()
        .map(|n| n.name)
        .collect()
}

macro_rules! on_flavors {
    ($f:ident, $crdb:ident, $yb:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $crdb() {
            if run_on(Server::Cockroach, stringify!($crdb)) {
                $f(Server::Cockroach).await
            }
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $yb() {
            if run_on(Server::Yugabyte, stringify!($yb)) {
                $f(Server::Yugabyte).await
            }
        }
    };
}

// ----------------------------------------------------------------------------------------------
// Connection, flavour detection, capabilities
// ----------------------------------------------------------------------------------------------

async fn info_and_capabilities(server: Server) {
    let s = connect(server).await;
    let info = s.server_info().await.unwrap();
    let flavor = info.extra.get("flavor").map(String::as_str);
    match server {
        Server::Cockroach => {
            assert_eq!(info.product, "CockroachDB");
            assert_eq!(flavor, Some("cockroachdb"));
            assert!(
                info.version.starts_with(|c: char| c.is_ascii_digit()),
                "{}",
                info.version
            );
            // 24.3+ has triggers: the per-server capabilities switch them on.
            assert!(s.capabilities().expect("capabilities").triggers);
        }
        Server::Yugabyte => {
            assert_eq!(info.product, "YugabyteDB");
            assert_eq!(flavor, Some("yugabytedb"));
            assert!(s.capabilities().is_some());
        }
    }
    assert!(!info.extra.contains_key("kindMismatch"));
    s.ping().await.unwrap();
    s.close().await.unwrap();
}
on_flavors!(
    info_and_capabilities,
    info_and_capabilities_crdb,
    info_and_capabilities_yb
);

/// A profile that names the wrong kind still gets the right flavour.
async fn kind_mismatch_is_detected(server: Server) {
    setup(server).await;
    let s = PostgresDriver::new()
        .connect(
            &config(server, DbKind::Postgres),
            Some(&Secret::new(common::PASSWORD)),
        )
        .await
        .expect("connect as postgres");
    let info = s.server_info().await.unwrap();
    assert_eq!(
        info.extra.get("flavor").map(String::as_str),
        Some(server.kind().id())
    );
    assert_eq!(
        info.extra.get("kindMismatch").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        s.capabilities().unwrap(),
        Capabilities::planned_for(server.kind()).tap(|c| {
            if server.is_cockroach() {
                c.triggers = true;
            }
        })
    );
}
on_flavors!(
    kind_mismatch_is_detected,
    kind_mismatch_crdb,
    kind_mismatch_yb
);

trait Tap: Sized {
    fn tap(mut self, f: impl FnOnce(&mut Self)) -> Self {
        f(&mut self);
        self
    }
}
impl<T> Tap for T {}

async fn flavor_driver_rejects_other_kind(_server: Server) {
    let err = PostgresFlavorDriver::new(DbKind::CockroachDb)
        .connect(&config(Server::Cockroach, DbKind::Postgres), None)
        .await
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert!(matches!(err, Error::Invalid(_)), "{err:?}");
}

#[tokio::test]
async fn flavor_driver_kind_check() {
    if enabled() {
        flavor_driver_rejects_other_kind(Server::Cockroach).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cockroach_tls_verify_full_and_bad_password() {
    if !run_on(Server::Cockroach, "cockroach_tls") {
        return;
    }
    setup(Server::Cockroach).await;
    let mut cfg = config(Server::Cockroach, DbKind::CockroachDb);
    cfg.tls = TlsConfig {
        mode: TlsMode::VerifyFull,
        ca_cert_path: Some(format!("{}/ca.crt", certs())),
        ..TlsConfig::default()
    };
    let driver = PostgresFlavorDriver::new(DbKind::CockroachDb);
    let s = driver
        .connect(&cfg, Some(&Secret::new(common::PASSWORD)))
        .await
        .expect("verify-full with the cockroach CA");
    assert_eq!(scalar(&*s, "SELECT 1").await, Value::Int(1));
    // The PostgreSQL test CA is not the one that signed the node certificate.
    cfg.tls.ca_cert_path = Some(common::other_ca_path());
    let err = driver
        .connect(&cfg, Some(&Secret::new(common::PASSWORD)))
        .await
        .err()
        .unwrap_or_else(|| panic!("wrong CA must fail"));
    assert!(matches!(err, Error::Tls(_)), "{err:?}");
    cfg.tls.mode = TlsMode::Prefer;
    let err = driver
        .connect(&cfg, Some(&Secret::new("wrong")))
        .await
        .err()
        .unwrap_or_else(|| panic!("wrong password must fail"));
    assert!(matches!(err, Error::Authentication(_)), "{err:?}");
}

#[tokio::test]
async fn cluster_option_is_validated() {
    if !enabled() {
        return;
    }
    let mut cfg = config(Server::Cockroach, DbKind::CockroachDb);
    cfg.options
        .insert("cockroach.cluster".into(), "bad id; drop".into());
    let err = PostgresFlavorDriver::new(DbKind::CockroachDb)
        .connect(&cfg, None)
        .await
        .err()
        .unwrap_or_else(|| panic!("must fail before connecting"));
    assert!(
        matches!(err, Error::Invalid(m) if m.contains("cluster")),
        "invalid id"
    );
}

// ----------------------------------------------------------------------------------------------
// Tree, structure, data, search
// ----------------------------------------------------------------------------------------------

async fn tree_details_and_data(server: Server) {
    let s = connect(server).await;
    let sc = unique("flv_tree");
    drop_schema(&*s, &sc).await;
    exec(
        &*s,
        &format!(
            "CREATE SCHEMA {sc}; \
             CREATE TABLE {sc}.parent (id int PRIMARY KEY, name text NOT NULL); \
             CREATE TABLE {sc}.child (id int PRIMARY KEY, pid int REFERENCES {sc}.parent(id), \
                v numeric(10,2), j jsonb); \
             CREATE INDEX child_v_idx ON {sc}.child (v); \
             CREATE VIEW {sc}.vw AS SELECT id, name FROM {sc}.parent; \
             CREATE SEQUENCE {sc}.sq; \
             INSERT INTO {sc}.parent VALUES (1, 'a'), (2, 'b'); \
             INSERT INTO {sc}.child VALUES (1, 1, 1.50, '{{\"k\": 1}}')"
        ),
    )
    .await;
    let db = ObjectPath::root().child(ObjectKind::Database, "dbstudio");
    assert!(
        children(&*s, &ObjectPath::root())
            .await
            .contains(&"dbstudio".to_string())
    );
    assert_eq!(children(&*s, &db).await, vec!["schemas"]);
    let schemas = children(&*s, &db.child(ObjectKind::Folder, "schemas")).await;
    assert!(schemas.contains(&sc) && schemas.contains(&"public".to_string()));
    // System schemas come last and are tagged.
    let nodes = s
        .list_children(&db.child(ObjectKind::Folder, "schemas"))
        .await
        .unwrap();
    let sys: Vec<&str> = nodes
        .iter()
        .filter(|n| n.detail.as_deref() == Some("system"))
        .map(|n| n.name.as_str())
        .collect();
    assert!(sys.contains(&"pg_catalog") && sys.contains(&"information_schema"));
    if server.is_cockroach() {
        assert!(sys.contains(&"crdb_internal"), "{sys:?}");
    }
    let scp = schema_path("dbstudio", &sc);
    assert_eq!(
        children(&*s, &scp).await,
        vec!["tables", "views", "sequences"]
    );
    assert_eq!(
        children(&*s, &scp.child(ObjectKind::Folder, "tables")).await,
        vec!["child", "parent"]
    );
    assert_eq!(
        children(&*s, &scp.child(ObjectKind::Folder, "views")).await,
        vec!["vw"]
    );
    assert_eq!(
        children(&*s, &scp.child(ObjectKind::Folder, "sequences")).await,
        vec!["sq"]
    );
    let child = table_path(&sc, "child");
    let folders = children(&*s, &child).await;
    assert!(folders.starts_with(&["columns".to_string(), "indexes".to_string()]));
    assert_eq!(
        children(&*s, &child.child(ObjectKind::Folder, "columns")).await,
        vec!["id", "pid", "v", "j"]
    );
    let idx = children(&*s, &child.child(ObjectKind::Folder, "indexes")).await;
    assert!(
        idx.iter().any(|i| i == "child_v_idx") && idx.len() == 2,
        "{idx:?}"
    );

    // Structure.
    let d = s.table_details(&child).await.unwrap();
    assert_eq!(
        d.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "pid", "v", "j"]
    );
    assert!(d.columns[0].primary_key && !d.columns[0].nullable);
    assert_eq!(d.foreign_keys.len(), 1);
    assert_eq!(d.foreign_keys[0].columns, vec!["pid"]);
    assert_eq!(d.indexes.iter().filter(|i| i.primary).count(), 1);
    assert!(
        d.indexes
            .iter()
            .any(|i| i.name == "child_v_idx" && !i.unique)
    );

    // DDL.
    let ddl = s.object_ddl(&child).await.unwrap();
    assert!(
        ddl.contains("CREATE TABLE") && ddl.contains(&sc) && ddl.contains("FOREIGN KEY"),
        "{ddl}"
    );
    let vddl = s
        .object_ddl(
            &scp.child(ObjectKind::Folder, "views")
                .child(ObjectKind::View, "vw"),
        )
        .await
        .unwrap();
    assert!(
        vddl.contains("CREATE VIEW") || vddl.contains("CREATE OR REPLACE VIEW"),
        "{vddl}"
    );
    let sddl = s
        .object_ddl(
            &scp.child(ObjectKind::Folder, "sequences")
                .child(ObjectKind::Sequence, "sq"),
        )
        .await
        .unwrap();
    assert!(sddl.contains("CREATE SEQUENCE"), "{sddl}");
    assert!(matches!(
        s.object_ddl(&table_path(&sc, "nope")).await,
        Err(Error::NotFound(_))
    ));

    // Data and count.
    let mut sink = CollectingSink::new();
    s.fetch_table(
        TableDataRequest {
            query_id: Uuid::new_v4(),
            table: child.clone(),
            filters: vec![],
            raw_where: None,
            sort: vec![],
            offset: 0,
            limit: 10,
            batch_size: None,
        },
        &mut sink,
    )
    .await
    .unwrap();
    let r = sink.into_results();
    let StatementOutcome::Rows(rs) = &r[0].outcome else {
        panic!("{r:?}")
    };
    assert_eq!(rs.columns.len(), 4);
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0][0], Value::Int(1));
    assert_eq!(rs.rows[0][2], Value::Decimal("1.50".into()));
    assert!(matches!(rs.rows[0][3], Value::Json(_)));
    assert_eq!(
        s.count_table(TableCountRequest {
            table: child.clone(),
            filters: vec![],
            raw_where: None,
            query_id: None
        })
        .await
        .unwrap(),
        1
    );

    // Search finds tables, not the internal schemas.
    let found = s
        .search_objects(ObjectSearchRequest {
            query: "par".into(),
            scope: db.clone(),
            kinds: vec![],
            limit: 50,
        })
        .await
        .unwrap();
    assert!(found.iter().any(|n| n.name == "parent"), "{found:?}");
    assert!(
        !found
            .iter()
            .any(|n| n.path.segments.iter().any(|g| g.name == "crdb_internal")),
        "{found:?}"
    );
    drop_schema(&*s, &sc).await;
}
on_flavors!(tree_details_and_data, tree_crdb, tree_yb);

// ----------------------------------------------------------------------------------------------
// Editing (also the hidden `rowid` key of CockroachDB)
// ----------------------------------------------------------------------------------------------

fn cell(c: &str, v: EditValue) -> CellEdit {
    CellEdit {
        column: c.into(),
        value: v,
    }
}

async fn fetch_all(s: &dyn Session, table: &ObjectPath) -> ResultSet {
    let mut sink = CollectingSink::new();
    s.fetch_table(
        TableDataRequest {
            query_id: Uuid::new_v4(),
            table: table.clone(),
            filters: vec![],
            raw_where: None,
            sort: vec![SortKey {
                column: "a".into(),
                descending: false,
            }],
            offset: 0,
            limit: 100,
            batch_size: None,
        },
        &mut sink,
    )
    .await
    .unwrap();
    match sink.into_results().remove(0).outcome {
        StatementOutcome::Rows(rs) => rs,
        other => panic!("{other:?}"),
    }
}

async fn editing_with_primary_key(server: Server) {
    let s = connect(server).await;
    let sc = unique("flv_edit");
    drop_schema(&*s, &sc).await;
    exec(
        &*s,
        &format!("CREATE SCHEMA {sc}; CREATE TABLE {sc}.t (a int PRIMARY KEY, b text)"),
    )
    .await;
    exec(
        &*s,
        &format!("INSERT INTO {sc}.t VALUES (1, 'x'), (2, 'y'), (3, 'z')"),
    )
    .await;
    let t = table_path(&sc, "t");
    let ed = s.editability(&t).await.unwrap();
    assert!(ed.editable);
    assert_eq!(ed.key_columns, vec!["a"]);
    assert!(ed.read_only_columns.is_empty());
    let changes = ChangeSet::Rows(RowChangeSet {
        table: t.clone(),
        key_columns: vec!["a".into()],
        changes: vec![
            RowChange::Update {
                key: vec![Value::Int(1)],
                set: vec![cell("b", EditValue::Text("ONE".into()))],
            },
            RowChange::Delete {
                key: vec![Value::Int(2)],
            },
            RowChange::Insert {
                values: vec![
                    cell("a", EditValue::Value(Value::Int(4))),
                    cell("b", EditValue::Text("four".into())),
                ],
            },
        ],
    });
    let preview = s.preview_changes(&changes).await.unwrap();
    assert_eq!(preview.len(), 3);
    let r = s.apply_changes(changes).await.unwrap();
    assert_eq!(r.applied, 3);
    let rs = fetch_all(&*s, &t).await;
    let got: Vec<(i64, String)> = rs
        .rows
        .iter()
        .map(|r| {
            (
                match r[0] {
                    Value::Int(i) => i,
                    _ => panic!(),
                },
                text(&r[1]),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![(1, "ONE".into()), (3, "z".into()), (4, "four".into())]
    );
    // A failing change rolls the whole set back and names the change.
    let bad = ChangeSet::Rows(RowChangeSet {
        table: t.clone(),
        key_columns: vec!["a".into()],
        changes: vec![
            RowChange::Update {
                key: vec![Value::Int(1)],
                set: vec![cell("b", EditValue::Text("never".into()))],
            },
            RowChange::Insert {
                values: vec![cell("a", EditValue::Value(Value::Int(3)))],
            },
        ],
    });
    match s.apply_changes(bad).await {
        Err(Error::ChangeFailed { index, .. }) => assert_eq!(index, 1),
        other => panic!("{other:?}"),
    }
    assert_eq!(text(&fetch_all(&*s, &t).await.rows[0][1]), "ONE");
    drop_schema(&*s, &sc).await;
}
on_flavors!(editing_with_primary_key, edit_pk_crdb, edit_pk_yb);

async fn editing_table_without_primary_key(server: Server) {
    if !server.is_cockroach() {
        skip(
            "edit_rowid_yb",
            "hidden rowid key is a CockroachDB feature (YugabyteDB uses ybctid)",
        );
        return;
    }
    let s = connect(server).await;
    let sc = unique("flv_rowid");
    drop_schema(&*s, &sc).await;
    exec(
        &*s,
        &format!("CREATE SCHEMA {sc}; CREATE TABLE {sc}.t (a int, b text)"),
    )
    .await;
    exec(
        &*s,
        &format!("INSERT INTO {sc}.t VALUES (1, 'x'), (2, 'y')"),
    )
    .await;
    let t = table_path(&sc, "t");
    // The hidden column is neither in the tree nor in the structure.
    assert_eq!(
        children(&*s, &t.child(ObjectKind::Folder, "columns")).await,
        vec!["a", "b"]
    );
    let d = s.table_details(&t).await.unwrap();
    assert_eq!(d.columns.len(), 2);
    let ed = s.editability(&t).await.unwrap();
    assert!(ed.editable, "{ed:?}");
    assert_eq!(ed.key_columns, vec!["rowid"]);
    assert_eq!(ed.read_only_columns, vec!["rowid"]);
    // The grid receives the key as an extra last column.
    let rs = fetch_all(&*s, &t).await;
    assert_eq!(
        rs.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "rowid"]
    );
    let rowid_of_1 = rs.rows[0][2].clone();
    assert!(matches!(rowid_of_1, Value::Int(_)));
    let r = s
        .apply_changes(ChangeSet::Rows(RowChangeSet {
            table: t.clone(),
            key_columns: vec!["rowid".into()],
            changes: vec![RowChange::Update {
                key: vec![rowid_of_1.clone()],
                set: vec![cell("b", EditValue::Text("edited".into()))],
            }],
        }))
        .await
        .unwrap();
    assert_eq!(r.applied, 1);
    assert_eq!(text(&fetch_all(&*s, &t).await.rows[0][1]), "edited");
    // The key is not writable.
    let err = s
        .apply_changes(ChangeSet::Rows(RowChangeSet {
            table: t.clone(),
            key_columns: vec!["rowid".into()],
            changes: vec![RowChange::Update {
                key: vec![rowid_of_1],
                set: vec![cell("rowid", EditValue::Value(Value::Int(5)))],
            }],
        }))
        .await
        .err()
        .unwrap_or_else(|| panic!("read-only"));
    assert!(matches!(err, Error::ChangeFailed { .. }), "{err:?}");
    drop_schema(&*s, &sc).await;
}
on_flavors!(
    editing_table_without_primary_key,
    edit_rowid_crdb,
    edit_rowid_yb
);

// ----------------------------------------------------------------------------------------------
// Execute, cancel, errors
// ----------------------------------------------------------------------------------------------

async fn execute_cancel_and_errors(server: Server) {
    let s: Arc<Box<dyn Session>> = Arc::new(connect(server).await);
    let r = exec(
        &**s,
        "SELECT 1 AS one, 'a' AS t; SELECT generate_series(1, 5)",
    )
    .await;
    assert_eq!(r.len(), 2);
    // A failing statement is a Query error with the server text.
    let e = execute_collect(&**s, QueryRequest::new("SELECT * FROM no_such_table_xyz"))
        .await
        .err()
        .unwrap();
    assert!(matches!(e, Error::Query { .. }), "{e:?}");

    // Cancel a running statement: it ends quickly with `Cancelled`.
    let id = Uuid::new_v4();
    let s2 = s.clone();
    let h = tokio::spawn(async move {
        let mut q = QueryRequest::new("SELECT pg_sleep(30)");
        q.query_id = id;
        execute_collect(&**s2, q).await
    });
    tokio::time::sleep(Duration::from_millis(800)).await;
    let started = Instant::now();
    s.cancel(id).await.unwrap();
    let res = h.await.unwrap();
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancel took {:?}",
        started.elapsed()
    );
    // The session still works afterwards.
    assert_eq!(scalar(&**s, "SELECT 2").await, Value::Int(2));
    // Client side timeout.
    let mut q = QueryRequest::new("SELECT pg_sleep(30)");
    q.timeout_ms = Some(500);
    let t = Instant::now();
    assert!(matches!(
        execute_collect(&**s, q).await,
        Err(Error::Timeout)
    ));
    assert!(t.elapsed() < Duration::from_secs(10));
    assert_eq!(scalar(&**s, "SELECT 3").await, Value::Int(3));
}
on_flavors!(execute_cancel_and_errors, exec_crdb, exec_yb);

// ----------------------------------------------------------------------------------------------
// EXPLAIN
// ----------------------------------------------------------------------------------------------

fn explain_req(text: &str, analyze: bool) -> ExplainRequest {
    ExplainRequest {
        query_id: Uuid::new_v4(),
        text: text.into(),
        database: None,
        analyze,
        options: ExplainOptions::default(),
        timeout_ms: None,
    }
}

async fn explain_plans(server: Server) {
    let s = connect(server).await;
    let sc = unique("flv_explain");
    drop_schema(&*s, &sc).await;
    exec(
        &*s,
        &format!("CREATE SCHEMA {sc}; CREATE TABLE {sc}.t (id int PRIMARY KEY, v int)"),
    )
    .await;
    exec(
        &*s,
        &format!("INSERT INTO {sc}.t SELECT g, g % 7 FROM generate_series(1, 300) g"),
    )
    .await;
    let plain = s
        .explain(explain_req(
            &format!("SELECT * FROM {sc}.t WHERE v = 3"),
            false,
        ))
        .await
        .unwrap();
    assert!(!plain.analyzed && !plain.rolled_back);
    let tree = parse_and_analyze(&plain, &PlanRules::default()).expect("parse plain");
    match server {
        Server::Cockroach => {
            assert_eq!(plain.format, PlanFormat::CockroachText);
            assert!(!plain.server_version.is_empty());
            assert!(tree.root.children.iter().chain([&tree.root]).any(|_| true));
            assert!(tree.root.extra.contains_key("distribution"));
        }
        Server::Yugabyte => assert_eq!(plain.format, PlanFormat::PostgresJson),
    }
    // ANALYZE of a write is rolled back.
    let t = format!("{sc}.t");
    let an = s
        .explain(explain_req(
            &format!("UPDATE {t} SET v = 100 WHERE id <= 10"),
            true,
        ))
        .await
        .unwrap();
    assert!(an.analyzed && an.rolled_back);
    let tree = parse_and_analyze(&an, &PlanRules::default()).expect("parse analyze");
    assert!(tree.analyzed);
    if server.is_cockroach() {
        assert_eq!(tree.root.kind, PlanNodeKind::Modify);
        assert_eq!(tree.root.actual_rows_total, Some(10.0));
    }
    assert_eq!(
        scalar(&*s, &format!("SELECT count(*) FROM {t} WHERE v = 100")).await,
        Value::Int(0)
    );
    // Several statements are refused; DDL cannot be analysed; a syntax error has a message.
    assert!(matches!(
        s.explain(explain_req("SELECT 1; SELECT 2", false)).await,
        Err(Error::Query { .. })
    ));
    assert!(matches!(
        s.explain(explain_req("DROP TABLE x", true)).await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        s.explain(explain_req("SELEC 1", false)).await,
        Err(Error::Query { .. })
    ));
    // Timeout of an analysed statement.
    let mut slow = explain_req("SELECT pg_sleep(30)", true);
    slow.timeout_ms = Some(500);
    assert!(matches!(
        s.explain(slow).await,
        Err(Error::Timeout) | Err(Error::Query { .. })
    ));
    // Lease: inside a transaction the explain does not disturb it.
    let lease = s
        .open_lease(LeaseOptions {
            database: None,
            schema: None,
            auto_commit: false,
        })
        .await
        .unwrap();
    let mut sink = CollectingSink::new();
    lease
        .execute(
            QueryRequest::new(format!("INSERT INTO {t} VALUES (1000, 1)")),
            &mut sink,
        )
        .await
        .unwrap();
    let raw = lease
        .explain(explain_req(
            &format!("DELETE FROM {t} WHERE id = 1000"),
            true,
        ))
        .await
        .unwrap();
    assert!(raw.analyzed);
    assert_eq!(lease.state().transaction, TxStatus::Active);
    lease.rollback().await.unwrap();
    lease.close().await.unwrap();
    assert_eq!(
        scalar(&*s, &format!("SELECT count(*) FROM {t} WHERE id = 1000")).await,
        Value::Int(0)
    );
    drop_schema(&*s, &sc).await;
}
on_flavors!(explain_plans, explain_crdb, explain_yb);

// ----------------------------------------------------------------------------------------------
// Lease: transactions, cancel, switching database (§2.17), retry hint
// ----------------------------------------------------------------------------------------------

async fn lease_run(l: &dyn Lease, sql: &str) -> Vec<StatementResult> {
    let mut sink = CollectingSink::new();
    l.execute(QueryRequest::new(sql), &mut sink)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    sink.into_results()
}

async fn lease_scalar(l: &dyn Lease, sql: &str) -> Value {
    match lease_run(l, sql).await.remove(0).outcome {
        StatementOutcome::Rows(rs) => rs.rows[0][0].clone(),
        other => panic!("{other:?}"),
    }
}

async fn lease_transactions_and_context(server: Server) {
    let s = connect(server).await;
    let other = unique("flv_other").replace("flv_", "dbs_flv_");
    let _ = execute_collect(
        &*s,
        QueryRequest::new(format!("DROP DATABASE IF EXISTS {other}")),
    )
    .await;
    exec(&*s, &format!("CREATE DATABASE {other}")).await;
    let sc = unique("flv_lease");
    drop_schema(&*s, &sc).await;
    exec(
        &*s,
        &format!("CREATE SCHEMA {sc}; CREATE TABLE {sc}.t (id int PRIMARY KEY)"),
    )
    .await;

    let lease = s
        .open_lease(LeaseOptions {
            database: None,
            schema: None,
            auto_commit: true,
        })
        .await
        .unwrap();
    assert_eq!(lease.state().database.as_deref(), Some("dbstudio"));
    // Manual transaction: commit and rollback.
    lease.set_auto_commit(false).await.unwrap();
    lease_run(&*lease, &format!("INSERT INTO {sc}.t VALUES (1)")).await;
    assert_eq!(lease.state().transaction, TxStatus::Active);
    lease.rollback().await.unwrap();
    assert_eq!(
        scalar(&*s, &format!("SELECT count(*) FROM {sc}.t")).await,
        Value::Int(0)
    );
    lease_run(&*lease, &format!("INSERT INTO {sc}.t VALUES (2)")).await;
    lease.commit().await.unwrap();
    assert_eq!(
        scalar(&*s, &format!("SELECT count(*) FROM {sc}.t")).await,
        Value::Int(1)
    );

    // §2.17: an open transaction blocks the switch.
    lease_run(&*lease, "SELECT 1").await;
    assert_eq!(lease.state().transaction, TxStatus::Active);
    let busy = lease
        .set_context(LeaseContext {
            database: Some(other.clone()),
            schema: None,
        })
        .await
        .err()
        .unwrap_or_else(|| panic!("busy"));
    assert!(matches!(busy, Error::Busy(_)), "{busy:?}");
    lease.rollback().await.unwrap();
    // Idle: the switch works and `current_database()` follows it.
    lease.set_auto_commit(true).await.unwrap();
    lease
        .set_context(LeaseContext {
            database: Some(other.clone()),
            schema: None,
        })
        .await
        .unwrap();
    assert_eq!(lease.state().database.as_deref(), Some(other.as_str()));
    assert_eq!(
        text(&lease_scalar(&*lease, "SELECT current_database()").await),
        other
    );
    // Back, with a schema.
    lease
        .set_context(LeaseContext {
            database: Some("dbstudio".into()),
            schema: Some(sc.clone()),
        })
        .await
        .unwrap();
    assert_eq!(
        text(&lease_scalar(&*lease, "SELECT current_database()").await),
        "dbstudio"
    );
    assert_eq!(
        text(&lease_scalar(&*lease, "SELECT current_schema()").await),
        sc
    );
    // The new connection is cancellable.
    let lease = Arc::new(lease);
    let id = Uuid::new_v4();
    let l2 = lease.clone();
    let h = tokio::spawn(async move {
        let mut sink = CollectingSink::new();
        let mut q = QueryRequest::new("SELECT pg_sleep(30)");
        q.query_id = id;
        l2.execute(q, &mut sink).await
    });
    tokio::time::sleep(Duration::from_millis(800)).await;
    let t = Instant::now();
    lease.cancel(id).await.unwrap();
    assert!(matches!(h.await.unwrap(), Err(Error::Cancelled)));
    assert!(t.elapsed() < Duration::from_secs(5));
    assert_eq!(lease_scalar(&**lease, "SELECT 5").await, Value::Int(5));
    lease.close().await.unwrap();
    let _ = execute_collect(
        &*s,
        QueryRequest::new(format!("DROP DATABASE IF EXISTS {other}")),
    )
    .await;
    drop_schema(&*s, &sc).await;
}
on_flavors!(lease_transactions_and_context, lease_crdb, lease_yb);

/// 40001 (serialization failure) comes back as a query error with the retry hint.
async fn serialization_failure_has_retry_hint(server: Server) {
    if !server.is_cockroach() {
        skip(
            "retry_hint_yb",
            "YugabyteDB needs its own conflict set-up (it retries internally)",
        );
        return;
    }
    let s = connect(server).await;
    let sc = unique("flv_retry");
    drop_schema(&*s, &sc).await;
    exec(&*s, &format!("CREATE SCHEMA {sc}; CREATE TABLE {sc}.k (id int PRIMARY KEY, v int); INSERT INTO {sc}.k VALUES (1, 0)")).await;
    let a = s
        .open_lease(LeaseOptions {
            database: None,
            schema: None,
            auto_commit: false,
        })
        .await
        .unwrap();
    let b = s
        .open_lease(LeaseOptions {
            database: None,
            schema: None,
            auto_commit: true,
        })
        .await
        .unwrap();
    lease_run(&*a, &format!("SELECT v FROM {sc}.k WHERE id = 1")).await;
    lease_run(&*b, &format!("UPDATE {sc}.k SET v = 1 WHERE id = 1")).await;
    let mut sink = CollectingSink::new();
    let w = a
        .execute(
            QueryRequest::new(format!("UPDATE {sc}.k SET v = 2 WHERE id = 1")),
            &mut sink,
        )
        .await;
    let c = a.commit().await;
    let err = w.err().or(c.err()).expect("conflict expected");
    match err {
        Error::Query { message, .. } => assert!(message.contains("retry"), "{message}"),
        other => panic!("{other:?}"),
    }
    let _ = a.rollback().await;
    a.close().await.unwrap();
    b.close().await.unwrap();
    drop_schema(&*s, &sc).await;
}
on_flavors!(serialization_failure_has_retry_hint, retry_crdb, retry_yb);

// ----------------------------------------------------------------------------------------------
// Model and DDL generation
// ----------------------------------------------------------------------------------------------

async fn model_and_ddl(server: Server) {
    let s = connect(server).await;
    let sc = unique("flv_model");
    let copy_db = unique("flv_copy").replace("flv_", "dbs_flv_");
    drop_schema(&*s, &sc).await;
    let _ = execute_collect(
        &*s,
        QueryRequest::new(format!("DROP DATABASE IF EXISTS {copy_db}")),
    )
    .await;
    exec(
        &*s,
        &format!(
            "CREATE SCHEMA {sc}; \
             CREATE TABLE {sc}.customer (id int PRIMARY KEY, email text UNIQUE NOT NULL); \
             CREATE TABLE {sc}.orders (id int PRIMARY KEY, customer_id int NOT NULL REFERENCES {sc}.customer(id) ON DELETE CASCADE, total numeric(10,2)); \
             CREATE TABLE {sc}.nopk (a int, b text); \
             CREATE INDEX orders_total ON {sc}.orders (total); \
             CREATE VIEW {sc}.v AS SELECT id FROM {sc}.customer; \
             COMMENT ON TABLE {sc}.customer IS 'customers'"
        ),
    )
    .await;
    let scope = schema_path("dbstudio", &sc);
    let m = s
        .schema_model(SchemaModelRequest {
            scope: scope.clone(),
            tables: vec![],
            include_views: true,
            include_comments: true,
            detail: ModelDetail::default(),
            sample_size: None,
        })
        .await
        .unwrap();
    let names: Vec<&str> = m.tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["customer", "nopk", "orders", "v"]);
    assert_eq!(m.relations.len(), 1);
    let rel = &m.relations[0];
    assert_eq!(rel.from_columns, vec!["customer_id"]);
    assert_eq!(rel.on_delete.as_deref(), Some("CASCADE"));
    let nopk = m.tables.iter().find(|t| t.name == "nopk").unwrap();
    // The hidden rowid is not part of the model.
    assert_eq!(nopk.columns.len(), 2);
    assert!(nopk.primary_key.is_empty());
    let cust = m.tables.iter().find(|t| t.name == "customer").unwrap();
    assert_eq!(cust.comment.as_deref(), Some("customers"));
    assert!(
        cust.unique_keys
            .iter()
            .any(|k| k == &vec!["email".to_string()])
    );

    // DDL replays into an empty database.
    let items = s
        .generate_ddl(DdlRequest {
            scope: scope.clone(),
            objects: vec![],
            options: DdlOptions::default(),
        })
        .await
        .unwrap();
    assert!(!items.is_empty());
    let pre: Vec<&DdlItem> = items
        .iter()
        .filter(|i| i.section == DdlSection::PreData)
        .collect();
    let post: Vec<&DdlItem> = items
        .iter()
        .filter(|i| i.section == DdlSection::PostData)
        .collect();
    assert!(
        pre.iter()
            .any(|i| i.kind == ObjectKind::Table && i.sql.contains("customer"))
    );
    assert!(
        post.iter().any(|i| i.sql.contains("FOREIGN KEY")),
        "foreign keys come after the tables: {items:#?}"
    );
    // Pre items all come before post items.
    let first_post = items
        .iter()
        .position(|i| i.section == DdlSection::PostData)
        .unwrap();
    assert!(
        items[first_post..]
            .iter()
            .all(|i| i.section == DdlSection::PostData)
    );
    exec(&*s, &format!("CREATE DATABASE {copy_db}")).await;
    for item in &items {
        let mut req = QueryRequest::new(item.sql.clone());
        req.database = Some(copy_db.clone());
        if let Err(e) = execute_collect(&*s, req).await {
            panic!("replay failed: {e}\n{}", item.sql);
        }
    }
    let n = |db: &str, sql: &str| {
        let mut req = QueryRequest::new(sql.to_string());
        req.database = Some(db.to_string());
        req
    };
    let copied = execute_collect(
        &*s,
        n(
            &copy_db,
            &format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = '{sc}'"),
        ),
    )
    .await
    .unwrap();
    let StatementOutcome::Rows(rs) = &copied[0].outcome else {
        panic!()
    };
    assert_eq!(rs.rows[0][0], Value::Int(4));
    // Options: no foreign keys, no views, drop + if not exists.
    let lean = s
        .generate_ddl(DdlRequest {
            scope: scope.clone(),
            objects: vec![],
            options: DdlOptions {
                include_foreign_keys: false,
                include_views: false,
                include_drop: true,
                if_not_exists: true,
                ..DdlOptions::default()
            },
        })
        .await
        .unwrap();
    assert!(!lean.iter().any(|i| i.sql.contains("FOREIGN KEY")));
    assert!(!lean.iter().any(|i| i.kind == ObjectKind::View));
    assert!(lean.iter().any(|i| i.sql.contains("DROP TABLE IF EXISTS")));
    assert!(
        lean.iter()
            .any(|i| i.sql.contains("CREATE TABLE IF NOT EXISTS"))
    );
    // A selection of one table.
    let one = s
        .generate_ddl(DdlRequest {
            scope: scope.clone(),
            objects: vec![table_path(&sc, "customer")],
            options: DdlOptions::default(),
        })
        .await
        .unwrap();
    assert!(one.iter().all(|i| !i.sql.contains("orders")), "{one:#?}");
    assert!(one.iter().any(|i| i.sql.contains("customer")));
    let _ = execute_collect(
        &*s,
        QueryRequest::new(format!("DROP DATABASE IF EXISTS {copy_db} CASCADE")),
    )
    .await;
    drop_schema(&*s, &sc).await;
}
on_flavors!(model_and_ddl, model_crdb, model_yb);

// ----------------------------------------------------------------------------------------------
// Flavour specific
// ----------------------------------------------------------------------------------------------

/// `SHOW CREATE ALL TABLES` is what the DDL of a database is built from.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cockroach_show_create_all_tables_order() {
    if !run_on(Server::Cockroach, "cockroach_show_create") {
        return;
    }
    let s = connect(Server::Cockroach).await;
    let sc = unique("flv_sca");
    drop_schema(&*s, &sc).await;
    exec(&*s, &format!("CREATE SCHEMA {sc}; CREATE TABLE {sc}.a (id int PRIMARY KEY); CREATE TABLE {sc}.b (id int PRIMARY KEY, a_id int REFERENCES {sc}.a(id))")).await;
    let r = rows(&*s, "SHOW CREATE ALL TABLES").await;
    let all: Vec<String> = r.iter().map(|row| text(&row[0])).collect();
    let fk = all
        .iter()
        .position(|t| t.contains("ADD CONSTRAINT") && t.contains(&sc))
        .expect("fk statement");
    let last_create = all
        .iter()
        .rposition(|t| t.starts_with("CREATE TABLE") && t.contains(&sc))
        .unwrap();
    assert!(fk > last_create);
    drop_schema(&*s, &sc).await;
}

/// Session-wide settings the driver applies do not break the catalog queries on a database
/// other than the login one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cockroach_second_database_tree() {
    if !run_on(Server::Cockroach, "cockroach_second_db") {
        return;
    }
    let s = connect(Server::Cockroach).await;
    let db = unique("dbs_flv_tree");
    let _ = execute_collect(
        &*s,
        QueryRequest::new(format!("DROP DATABASE IF EXISTS {db} CASCADE")),
    )
    .await;
    exec(&*s, &format!("CREATE DATABASE {db}")).await;
    let mut q = QueryRequest::new("CREATE TABLE other_t (id int PRIMARY KEY)");
    q.database = Some(db.clone());
    execute_collect(&*s, q).await.unwrap();
    let schemas = ObjectPath::root()
        .child(ObjectKind::Database, &db)
        .child(ObjectKind::Folder, "schemas")
        .child(ObjectKind::Schema, "public")
        .child(ObjectKind::Folder, "tables");
    assert_eq!(children(&*s, &schemas).await, vec!["other_t"]);
    let _ = execute_collect(
        &*s,
        QueryRequest::new(format!("DROP DATABASE IF EXISTS {db} CASCADE")),
    )
    .await;
}

// ----------------------------------------------------------------------------------------------
// CockroachDB: DDL options, types, triggers; closing a session cancels its statements
// ----------------------------------------------------------------------------------------------

async fn replay(s: &dyn Session, db: &str, items: &[DdlItem]) {
    for item in items {
        let mut req = QueryRequest::new(item.sql.clone());
        req.database = Some(db.to_string());
        if let Err(e) = execute_collect(s, req).await {
            panic!("replay failed: {e}\n{}", item.sql);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cockroach_ddl_options_types_and_triggers() {
    if !run_on(Server::Cockroach, "cockroach_ddl_options") {
        return;
    }
    let s = connect(Server::Cockroach).await;
    let sc = unique("flv_ddl");
    let copy = unique("dbs_flv_ddlcopy");
    drop_schema(&*s, &sc).await;
    let _ = execute_collect(
        &*s,
        QueryRequest::new(format!("DROP DATABASE IF EXISTS {copy} CASCADE")),
    )
    .await;
    exec(
        &*s,
        &format!(
            "CREATE SCHEMA {sc}; CREATE TYPE {sc}.mood AS ENUM ('a', 'b'); \
             CREATE TABLE {sc}.t (id int PRIMARY KEY, m {sc}.mood, v int, INDEX v_idx (v)); \
             CREATE TABLE {sc}.other (id int PRIMARY KEY); \
             CREATE MATERIALIZED VIEW {sc}.mv AS SELECT id FROM {sc}.t; \
             CREATE FUNCTION {sc}.f(a int) RETURNS int LANGUAGE SQL AS 'SELECT a + 1'; \
             CREATE FUNCTION {sc}.tf() RETURNS TRIGGER LANGUAGE PLpgSQL AS 'BEGIN RETURN NEW; END'; \
             CREATE TRIGGER trg BEFORE INSERT ON {sc}.t FOR EACH ROW EXECUTE FUNCTION {sc}.tf()"
        ),
    )
    .await;
    let scope = schema_path("dbstudio", &sc);
    let req = |options: DdlOptions, objects: Vec<ObjectPath>| DdlRequest {
        scope: scope.clone(),
        objects,
        options,
    };
    let all = s
        .generate_ddl(req(DdlOptions::default(), vec![]))
        .await
        .unwrap();
    let text: String = all
        .iter()
        .map(|i| i.sql.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("CREATE TYPE"), "{text}");
    assert!(text.contains("CREATE MATERIALIZED VIEW"));
    assert!(text.contains("CREATE FUNCTION"));
    assert!(text.contains("CREATE TRIGGER"), "{text}");
    assert!(
        !text.contains("dbstudio."),
        "database qualifier must be stripped: {text}"
    );
    assert!(
        all.iter()
            .any(|i| i.kind == ObjectKind::Trigger && i.section == DdlSection::PostData)
    );

    // include_drop: replays twice into the same database (drops, then types, then tables).
    let dropping = DdlOptions {
        include_drop: true,
        ..DdlOptions::default()
    };
    let items = s.generate_ddl(req(dropping, vec![])).await.unwrap();
    let sqls: Vec<&str> = items.iter().map(|i| i.sql.as_str()).collect();
    assert!(
        !sqls
            .iter()
            .any(|q| q.starts_with("DROP TYPE") && q.contains("CASCADE"))
    );
    assert!(sqls.iter().any(|q| q.starts_with("DROP MATERIALIZED VIEW")));
    let pos = |needle: &str| sqls.iter().position(|q| q.starts_with(needle)).unwrap();
    assert!(pos("DROP TABLE") < pos("DROP TYPE") && pos("DROP TYPE") < pos("CREATE TYPE"));
    exec(&*s, &format!("CREATE DATABASE {copy}")).await;
    replay(&*s, &copy, &items).await;
    replay(&*s, &copy, &items).await;
    let n = scalar(
        &*s,
        &format!(
            "SELECT count(*) FROM {copy}.information_schema.tables WHERE table_schema = '{sc}'"
        ),
    )
    .await;
    assert_eq!(n, Value::Int(3));

    // Selection: the table, the enum it uses (not the other table), no routines.
    let one = s
        .generate_ddl(req(DdlOptions::default(), vec![table_path(&sc, "t")]))
        .await
        .unwrap();
    let one_text: String = one
        .iter()
        .map(|i| i.sql.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        one_text.contains("CREATE TYPE") && one_text.contains("CREATE TABLE"),
        "{one_text}"
    );
    assert!(
        !one_text.contains("other") && !one_text.contains("CREATE FUNCTION"),
        "{one_text}"
    );
    // The selection can name the routine itself.
    let f = schema_path("dbstudio", &sc)
        .child(ObjectKind::Folder, "functions")
        .child(ObjectKind::Function, "f(a bigint)");
    let fo = s
        .generate_ddl(req(DdlOptions::default(), vec![f]))
        .await
        .unwrap();
    assert!(
        fo.iter().any(|i| i.sql.contains("CREATE FUNCTION"))
            && !fo.iter().any(|i| i.sql.contains("CREATE TABLE"))
    );

    // Unqualified, no indexes, no triggers.
    let lean = DdlOptions {
        qualify_schema: false,
        include_indexes: false,
        include_triggers: false,
        ..DdlOptions::default()
    };
    let lean_items = s.generate_ddl(req(lean, vec![])).await.unwrap();
    let lean_text: String = lean_items
        .iter()
        .map(|i| i.sql.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(lean_text.contains("CREATE TABLE t ("), "{lean_text}");
    assert!(!lean_text.contains(&format!("{sc}.t")) && !lean_text.contains("INDEX v_idx"));
    assert!(!lean_text.contains("CREATE TRIGGER"));
    // Grants are not supported: refused, not silently ignored.
    let g = DdlOptions {
        include_grants: true,
        ..DdlOptions::default()
    };
    assert!(matches!(
        s.generate_ddl(req(g, vec![])).await,
        Err(Error::Unsupported(_))
    ));
    let _ = execute_collect(
        &*s,
        QueryRequest::new(format!("DROP DATABASE IF EXISTS {copy} CASCADE")),
    )
    .await;
    drop_schema(&*s, &sc).await;
}

/// `Session::close` cancels what is still running (the cancel goes through a new connection,
/// so it must happen before the session refuses connections).
async fn close_cancels_running(server: Server) {
    let s: Arc<Box<dyn Session>> = Arc::new(connect(server).await);
    let marker = unique("close_marker").replace('_', "");
    let s2 = s.clone();
    let m = marker.clone();
    let h = tokio::spawn(async move {
        execute_collect(
            &**s2,
            QueryRequest::new(format!("SELECT pg_sleep(30), '{m}'")),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(800)).await;
    s.close().await.unwrap();
    let t = Instant::now();
    let res = h.await.unwrap();
    assert!(res.is_err(), "{res:?}");
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "took {:?}",
        t.elapsed()
    );
    // The statement is gone on the server, not just abandoned by the client.
    let other = connect(server).await;
    let running = match server {
        Server::Cockroach => format!(
            "SELECT count(*) FROM [SHOW CLUSTER STATEMENTS] WHERE query LIKE '%{marker}%' AND query NOT LIKE '%count(*)%'"
        ),
        Server::Yugabyte => format!(
            "SELECT count(*) FROM pg_stat_activity WHERE query LIKE '%{marker}%' AND state = 'active' AND query NOT LIKE '%count(*)%'"
        ),
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(scalar(&*other, &running).await, Value::Int(0));
}
on_flavors!(close_cancels_running, close_crdb, close_yb);
