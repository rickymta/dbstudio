//! Shared helpers of the integration tests (they only run with `DBSTUDIO_IT=1`, against the
//! `oraclefree` service from inside the `oracle-it` container, or on a machine with Instant Client
//! and `ORACLE_CLIENT_LIB_DIR` set).
#![allow(dead_code)]

use std::future::Future;
use std::sync::Once;
use std::time::Duration;

use dbstudio_core::{
    ColumnMeta, ConnectionConfig, DbKind, Driver, Error, ObjectKind, ObjectNode, ObjectPath,
    QueryRequest, Result, ResultSink, Secret, Session, StatementEnd, StatementOutcome,
    StatementResult, Value, execute_collect,
};
use dbstudio_driver_oracle::OracleDriver;
use uuid::Uuid;

pub const PASSWORD: &str = "dbstudio_test";

pub fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").as_deref() == Ok("1")
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

pub fn host() -> String {
    env_or("DBSTUDIO_IT_ORACLE_HOST", "127.0.0.1")
}

pub fn port() -> u16 {
    env_or("DBSTUDIO_IT_ORACLE_PORT", "15213")
        .parse()
        .expect("DBSTUDIO_IT_ORACLE_PORT")
}

pub fn service() -> String {
    env_or("DBSTUDIO_IT_ORACLE_SERVICE", "FREEPDB1")
}

static INIT: Once = Once::new();

/// Point the driver at the client directory of the environment (the container finds it through
/// ldconfig; on a Mac `ORACLE_CLIENT_LIB_DIR` names the Instant Client folder).
pub fn init_client() {
    INIT.call_once(|| {
        if let Ok(dir) = std::env::var("ORACLE_CLIENT_LIB_DIR") {
            dbstudio_driver_oracle::configure_client(Some(&dir));
        }
    });
}

pub fn config(user: &str) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new(DbKind::Oracle, "it", host());
    cfg.port = port();
    cfg.database = Some(service());
    cfg.username = Some(user.to_string());
    cfg.connect_timeout_ms = Some(15_000);
    cfg
}

pub async fn try_connect(cfg: &ConnectionConfig, password: &str) -> Result<Box<dyn Session>> {
    init_client();
    OracleDriver::new()
        .connect(cfg, Some(&Secret::new(password)))
        .await
}

/// Session of the application user `dbstudio` (owns the test objects).
pub async fn app() -> Box<dyn Session> {
    try_connect(&config("dbstudio"), PASSWORD)
        .await
        .expect("connect as dbstudio")
}

/// Session of `SYSTEM` (dictionary and `V$` access).
pub async fn system() -> Box<dyn Session> {
    try_connect(&config("SYSTEM"), PASSWORD)
        .await
        .expect("connect as SYSTEM")
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

/// Drop an object ignoring "does not exist".
pub async fn drop_object(s: &dyn Session, kind: &str, name: &str) {
    let extra = if kind == "TABLE" {
        " CASCADE CONSTRAINTS PURGE"
    } else {
        ""
    };
    let sql = format!(
        "BEGIN EXECUTE IMMEDIATE 'DROP {kind} {name}{extra}'; \
         EXCEPTION WHEN OTHERS THEN IF SQLCODE NOT IN (-942, -4043, -1434, -2289, -12003, -4080) THEN RAISE; END IF; END;"
    );
    exec(s, &sql).await;
}

/// Sink that records everything.
#[derive(Default)]
pub struct Rec {
    pub cols: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Value>>,
    pub batches: Vec<usize>,
    pub messages: Vec<String>,
    pub ends: Vec<StatementEnd>,
    pub starts: Vec<dbstudio_core::StatementStart>,
}

impl ResultSink for Rec {
    fn on_columns(&mut self, columns: Vec<ColumnMeta>) -> Result<()> {
        self.cols = columns;
        Ok(())
    }
    fn on_rows(&mut self, mut rows: Vec<Vec<Value>>) -> Result<()> {
        self.batches.push(rows.len());
        self.rows.append(&mut rows);
        Ok(())
    }
    fn on_message(&mut self, message: String) -> Result<()> {
        self.messages.push(message);
        Ok(())
    }
    fn on_statement_end(&mut self, end: StatementEnd) -> Result<()> {
        self.ends.push(end);
        Ok(())
    }
    fn on_statement_start(&mut self, start: dbstudio_core::StatementStart) -> Result<()> {
        self.starts.push(start);
        Ok(())
    }
}

impl Rec {
    pub fn col(&self, name: &str) -> usize {
        self.cols
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("no column {name}"))
    }
    pub fn get(&self, row: usize, name: &str) -> &Value {
        &self.rows[row][self.col(name)]
    }
}

/// Await `work` while cancelling `id` repeatedly until it ends (robust against a cancel that
/// arrives before the statement runs).
pub async fn cancel_until_done<T>(s: &dyn Session, id: Uuid, work: impl Future<Output = T>) -> T {
    tokio::pin!(work);
    loop {
        tokio::select! {
            r = &mut work => return r,
            _ = tokio::time::sleep(Duration::from_millis(300)) => { s.cancel(id).await.unwrap(); }
        }
    }
}

/// Names of the nodes.
pub fn names(nodes: &[ObjectNode]) -> Vec<String> {
    nodes.iter().map(|n| n.name.clone()).collect()
}

pub fn schema_path(schema: &str) -> ObjectPath {
    ObjectPath::root().child(ObjectKind::Schema, schema)
}

pub fn table_path(schema: &str, name: &str) -> ObjectPath {
    schema_path(schema)
        .child(ObjectKind::Folder, "tables")
        .child(ObjectKind::Table, name)
}

pub fn int(v: i64) -> Value {
    Value::Int(v)
}

pub fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

pub fn dec(s: &str) -> Value {
    Value::Decimal(s.to_string())
}
