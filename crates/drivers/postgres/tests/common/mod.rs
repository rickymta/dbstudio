//! Shared helpers for the integration tests (only run with `DBSTUDIO_IT=1`).
#![allow(dead_code)]

use std::time::Instant;

use dbstudio_core::{
    ColumnMeta, ConnectionConfig, DbKind, Driver, Error, ObjectKind, ObjectNode, ObjectPath,
    QueryRequest, Result, ResultSink, Secret, Session, StatementEnd, StatementOutcome,
    StatementResult, TlsConfig, TlsMode, Value, execute_collect,
};
use dbstudio_driver_postgres::PostgresDriver;
use tokio::sync::OnceCell;

pub const PG16: u16 = 54316;
pub const PG12: u16 = 54312;
pub const PASSWORD: &str = "dbstudio_test";

pub fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").as_deref() == Ok("1")
}

pub fn certs_dir() -> String {
    format!("{}/../../../docker/certs", env!("CARGO_MANIFEST_DIR"))
}

pub fn ca_path() -> String {
    format!("{}/ca.crt", certs_dir())
}

pub fn other_ca_path() -> String {
    format!("{}/other-ca.crt", certs_dir())
}

pub fn config(port: u16, mode: TlsMode) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new(DbKind::Postgres, "it", "127.0.0.1");
    cfg.port = port;
    cfg.database = Some("dbstudio".into());
    cfg.username = Some("dbstudio".into());
    cfg.tls = TlsConfig {
        mode,
        ca_cert_path: if matches!(mode, TlsMode::VerifyCa | TlsMode::VerifyFull) {
            Some(ca_path())
        } else {
            None
        },
        ..TlsConfig::default()
    };
    cfg.connect_timeout_ms = Some(5000);
    cfg
}

pub async fn try_connect(cfg: &ConnectionConfig, password: &str) -> Result<Box<dyn Session>> {
    PostgresDriver::new()
        .connect(cfg, Some(&Secret::new(password)))
        .await
}

/// Plain connection without TLS requirements (`Prefer` works on both servers).
pub async fn connect(port: u16) -> Box<dyn Session> {
    try_connect(&config(port, TlsMode::Prefer), PASSWORD)
        .await
        .expect("connect")
}

pub async fn exec(s: &dyn Session, sql: &str) -> Vec<StatementResult> {
    match execute_collect(s, QueryRequest::new(sql)).await {
        Ok(r) => r,
        Err(e) => panic!("execute failed: {e}\n{sql}"),
    }
}

pub async fn exec_err(s: &dyn Session, sql: &str) -> Error {
    match execute_collect(s, QueryRequest::new(sql)).await {
        Ok(_) => panic!("expected an error for: {sql}"),
        Err(e) => e,
    }
}

/// Rows of the first result set of `sql`.
pub async fn rows(s: &dyn Session, sql: &str) -> Vec<Vec<Value>> {
    let results = exec(s, sql).await;
    match results.into_iter().next().map(|r| r.outcome) {
        Some(StatementOutcome::Rows(rs)) => rs.rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

pub async fn scalar(s: &dyn Session, sql: &str) -> Value {
    rows(s, sql).await.remove(0).remove(0)
}

/// Sink that records everything, including batch sizes and timing.
#[derive(Default)]
pub struct Rec {
    pub cols: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Value>>,
    pub batches: Vec<usize>,
    pub messages: Vec<String>,
    pub ends: Vec<StatementEnd>,
    pub first_rows_at: Option<Instant>,
    pub ended_at: Option<Instant>,
}

impl ResultSink for Rec {
    fn on_columns(&mut self, columns: Vec<ColumnMeta>) -> Result<()> {
        self.cols = columns;
        Ok(())
    }
    fn on_rows(&mut self, mut rows: Vec<Vec<Value>>) -> Result<()> {
        self.first_rows_at.get_or_insert_with(Instant::now);
        self.batches.push(rows.len());
        self.rows.append(&mut rows);
        Ok(())
    }
    fn on_message(&mut self, message: String) -> Result<()> {
        self.messages.push(message);
        Ok(())
    }
    fn on_statement_end(&mut self, end: StatementEnd) -> Result<()> {
        self.ended_at = Some(Instant::now());
        self.ends.push(end);
        Ok(())
    }
}

impl Rec {
    /// Column index by name.
    pub fn col(&self, name: &str) -> usize {
        self.cols
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("no column {name}"))
    }
    /// Value of column `name` in row `row`.
    pub fn get(&self, row: usize, name: &str) -> &Value {
        &self.rows[row][self.col(name)]
    }
}

pub fn table_path(schema: &str, kind: ObjectKind, name: &str) -> ObjectPath {
    let folder = match kind {
        ObjectKind::View => "views",
        ObjectKind::MaterializedView => "materializedViews",
        ObjectKind::Function => "functions",
        ObjectKind::Procedure => "procedures",
        ObjectKind::Sequence => "sequences",
        _ => "tables",
    };
    ObjectPath::root()
        .child(ObjectKind::Database, "dbstudio")
        .child(ObjectKind::Folder, "schemas")
        .child(ObjectKind::Schema, schema)
        .child(ObjectKind::Folder, folder)
        .child(kind, name)
}

pub fn schema_path(schema: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Database, "dbstudio")
        .child(ObjectKind::Folder, "schemas")
        .child(ObjectKind::Schema, schema)
}

pub fn names(nodes: &[ObjectNode]) -> Vec<String> {
    nodes.iter().map(|n| n.name.clone()).collect()
}

pub const SCHEMA: &str = "dbs_it";

const FIXTURE_SQL: &str = include_str!("fixture.sql");

static FIXTURE_16: OnceCell<()> = OnceCell::const_new();
static FIXTURE_12: OnceCell<()> = OnceCell::const_new();

/// Create the shared fixture (once per server per test process) and return a fresh session.
pub async fn fixture(port: u16) -> Box<dyn Session> {
    let cell = if port == PG16 {
        &FIXTURE_16
    } else {
        &FIXTURE_12
    };
    cell.get_or_init(|| async {
        let s = connect(port).await;
        // Terminate stragglers and recreate the secondary database.
        exec(
            &*s,
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = 'dbs_it_other'",
        )
        .await;
        exec(&*s, "DROP DATABASE IF EXISTS dbs_it_other").await;
        exec(&*s, "CREATE DATABASE dbs_it_other").await;
        let mut req = QueryRequest::new(
            "CREATE TABLE other_t (id int PRIMARY KEY, v text); INSERT INTO other_t VALUES (1, 'x')",
        );
        req.database = Some("dbs_it_other".into());
        execute_collect(&*s, req).await.expect("other db setup");
        exec(&*s, FIXTURE_SQL).await;
        s.close().await.ok();
    })
    .await;
    connect(port).await
}
