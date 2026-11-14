//! Shared helpers of the integration tests (`DBSTUDIO_IT=1`; a missing server skips the test
//! unless `DBSTUDIO_IT_STRICT=1`).

#![allow(dead_code)]

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use dbstudio_core::{
    CollectingSink, ConnectionConfig, DbKind, Driver, Lease, LeaseOptions, ObjectKind, ObjectPath,
    QueryRequest, ResultSet, Secret, Session, StatementOutcome, StatementResult, TlsMode, Value,
    execute_collect,
};
use dbstudio_driver_cassandra::{CassandraDriver, ScyllaDriver};

#[derive(Clone, Copy)]
pub struct Server {
    pub name: &'static str,
    pub port_env: &'static str,
    pub port: u16,
    pub scylla: bool,
}

pub const SCYLLA: Server = Server {
    name: "scylla",
    port_env: "DBSTUDIO_IT_SCYLLA_PORT",
    port: 19060,
    scylla: true,
};
pub const CASSANDRA5: Server = Server {
    name: "cassandra5",
    port_env: "DBSTUDIO_IT_CASSANDRA5_PORT",
    port: 19050,
    scylla: false,
};

pub const TLS_PORT_ENV: &str = "DBSTUDIO_IT_SCYLLA_TLS_PORT";
pub const SCYLLA_TLS_PORT: u16 = 19062;

pub fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").is_ok_and(|v| v == "1")
}

fn strict() -> bool {
    std::env::var("DBSTUDIO_IT_STRICT").is_ok_and(|v| v == "1")
}

pub fn port_of(s: Server) -> u16 {
    std::env::var(s.port_env)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(s.port)
}

fn reachable(port: u16) -> bool {
    ("127.0.0.1", port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .is_some_and(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok())
}

/// `true` when the test should run; `false` to skip (panics in strict mode).
pub fn probe(s: Server, port: u16) -> bool {
    if !enabled() {
        return false;
    }
    if reachable(port) {
        return true;
    }
    assert!(
        !strict(),
        "{} is not reachable on 127.0.0.1:{port} (DBSTUDIO_IT_STRICT=1)",
        s.name
    );
    eprintln!("skip: {} is not reachable on 127.0.0.1:{port}", s.name);
    false
}

pub fn kind_of(s: Server) -> DbKind {
    if s.scylla { DbKind::ScyllaDb } else { DbKind::Cassandra }
}

pub fn config(s: Server, user: &str) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new(kind_of(s), s.name, "127.0.0.1");
    cfg.port = port_of(s);
    cfg.username = Some(user.into());
    cfg.tls.mode = TlsMode::Disable;
    cfg.connect_timeout_ms = Some(30_000);
    cfg
}

pub fn secret_of(user: &str) -> Secret {
    Secret::new(if user == "cassandra" {
        "cassandra"
    } else {
        "dbstudio_test"
    })
}

pub async fn connect_cfg(s: Server, cfg: &ConnectionConfig, user: &str) -> dbstudio_core::Result<Box<dyn Session>> {
    let secret = secret_of(user);
    if s.scylla {
        ScyllaDriver::new().connect(cfg, Some(&secret)).await
    } else {
        CassandraDriver::new().connect(cfg, Some(&secret)).await
    }
}

/// Test context: a session as `dbstudio` and a lock that serialises the tests of one server.
pub struct Ctx {
    pub server: Server,
    pub session: Box<dyn Session>,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn ctx(s: Server) -> Option<Ctx> {
    if !probe(s, port_of(s)) {
        return None;
    }
    let guard = SERIAL.lock().await;
    let admin = connect_cfg(s, &config(s, "cassandra"), "cassandra")
        .await
        .expect("admin connect");
    run(
        admin.as_ref(),
        None,
        "CREATE ROLE IF NOT EXISTS dbstudio WITH PASSWORD = 'dbstudio_test' AND LOGIN = true AND SUPERUSER = true",
    )
    .await;
    admin.close().await.unwrap();
    let session = connect_cfg(s, &config(s, "dbstudio"), "dbstudio")
        .await
        .expect("connect as dbstudio");
    Some(Ctx {
        server: s,
        session,
        _guard: guard,
    })
}

impl Ctx {
    pub async fn keyspace(&self, name: &str) -> Keyspace {
        let ks = format!("it_{}_{name}", self.server.name);
        Keyspace::create(self.server, self.session.as_ref(), &ks).await
    }

    pub async fn lease(&self, ks: Option<&str>) -> Box<dyn Lease> {
        self.session
            .open_lease(LeaseOptions {
                database: ks.map(String::from),
                ..LeaseOptions::default()
            })
            .await
            .expect("open lease")
    }
}

/// A keyspace dropped when the guard goes out of scope (also when the test fails).
pub struct Keyspace {
    pub name: String,
    server: Server,
}

impl Keyspace {
    pub async fn create(server: Server, session: &dyn Session, name: &str) -> Keyspace {
        run(session, None, &format!("DROP KEYSPACE IF EXISTS {name}")).await;
        let tablets = if server.scylla { " AND tablets = {'enabled': false}" } else { "" };
        run(
            session,
            None,
            &format!(
                "CREATE KEYSPACE {name} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}{tablets}"
            ),
        )
        .await;
        Keyspace {
            name: name.to_string(),
            server,
        }
    }

    pub fn path(&self) -> ObjectPath {
        ObjectPath::root().child(ObjectKind::Database, &self.name)
    }

    pub fn table(&self, t: &str) -> ObjectPath {
        self.path()
            .child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, t)
    }

    pub fn view(&self, t: &str) -> ObjectPath {
        self.path()
            .child(ObjectKind::Folder, "materializedViews")
            .child(ObjectKind::MaterializedView, t)
    }
}

impl Drop for Keyspace {
    fn drop(&mut self) {
        let (server, name) = (self.server, self.name.clone());
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                if let Ok(s) = connect_cfg(server, &config(server, "cassandra"), "cassandra").await {
                    let _ = execute_collect(
                        s.as_ref(),
                        QueryRequest::new(format!("DROP KEYSPACE IF EXISTS {name}")),
                    )
                    .await;
                    let _ = s.close().await;
                }
            });
        })
        .join();
    }
}

/// Run a script (optionally in a keyspace) and return every statement result; panics on error.
pub async fn run(s: &dyn Session, ks: Option<&str>, text: &str) -> Vec<StatementResult> {
    try_run(s, ks, text)
        .await
        .unwrap_or_else(|e| panic!("`{text}` failed: {e}"))
}

pub async fn try_run(
    s: &dyn Session,
    ks: Option<&str>,
    text: &str,
) -> dbstudio_core::Result<Vec<StatementResult>> {
    let mut req = QueryRequest::new(text);
    req.database = ks.map(String::from);
    execute_collect(s, req).await
}

pub fn result_set(r: &StatementResult) -> &ResultSet {
    match &r.outcome {
        StatementOutcome::Rows(rs) => rs,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Rows of the first result of `text`.
pub async fn select(s: &dyn Session, ks: &str, text: &str) -> Vec<Vec<Value>> {
    let results = run(s, Some(ks), text).await;
    result_set(&results[0]).rows.clone()
}

pub fn text(s: &str) -> Value {
    Value::Text(s.into())
}

/// Run `text` on a lease, collecting the results.
pub async fn lease_run(l: &dyn Lease, text: &str) -> dbstudio_core::Result<Vec<StatementResult>> {
    let mut sink = CollectingSink::new();
    l.execute(QueryRequest::new(text), &mut sink).await?;
    Ok(sink.into_results())
}
