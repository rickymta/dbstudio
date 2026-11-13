//! Shared helpers of the integration tests (only active when `DBSTUDIO_IT=1`).
#![allow(dead_code)]

use std::sync::Arc;

use dbstudio_core::{
    ConnectionConfig, DbKind, Driver, Error, QueryRequest, Result, ResultSet, Secret, Session,
    StatementOutcome, StatementResult, TlsConfig, TlsMode, Value, execute_collect,
};
use dbstudio_driver_mssql::MssqlDriver;
use tokio::sync::OnceCell;

pub const IT_DB: &str = "dbstudio_it";
pub const PASSWORD: &str = "Dbstudio_test1!";

pub fn it_enabled() -> bool {
    std::env::var("DBSTUDIO_IT").is_ok_and(|v| v == "1")
}

pub fn port() -> u16 {
    std::env::var("DBSTUDIO_IT_MSSQL_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(14333)
}

pub fn config(db: Option<&str>, tls: TlsMode) -> ConnectionConfig {
    let mut c = ConnectionConfig::new(DbKind::SqlServer, "mssql-it", "127.0.0.1");
    c.port = port();
    c.username = Some("sa".into());
    c.database = db.map(str::to_string);
    c.tls = TlsConfig {
        mode: tls,
        ..Default::default()
    };
    c.connect_timeout_ms = Some(60_000);
    c
}

pub async fn connect_cfg(cfg: &ConnectionConfig) -> Result<Box<dyn Session>> {
    MssqlDriver::new()
        .connect(cfg, Some(&Secret::new(PASSWORD)))
        .await
}

pub async fn session_on(db: &str) -> Arc<dyn Session> {
    Arc::from(
        connect_cfg(&config(Some(db), TlsMode::Require))
            .await
            .unwrap_or_else(|e| panic!("connect to {db} failed: {e}")),
    )
}

pub async fn master() -> Arc<dyn Session> {
    session_on("master").await
}

static SETUP: OnceCell<()> = OnceCell::const_new();

/// Create (once per run) the test database with the fixture of `fixture.sql`.
pub async fn ensure_fixture() {
    SETUP
        .get_or_init(|| async {
            let m = master().await;
            run(
                &*m,
                &format!(
                    "IF DB_ID(N'{IT_DB}') IS NOT NULL BEGIN ALTER DATABASE [{IT_DB}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{IT_DB}]; END\nGO\nCREATE DATABASE [{IT_DB}] COLLATE Latin1_General_100_CI_AS\n"
                ),
            )
            .await;
            let s = session_on(IT_DB).await;
            run(&*s, include_str!("fixture.sql")).await;
            s.close().await.ok();
            m.close().await.ok();
        })
        .await;
}

pub async fn fixture() -> Arc<dyn Session> {
    ensure_fixture().await;
    session_on(IT_DB).await
}

pub async fn run(s: &dyn Session, sql: &str) -> Vec<StatementResult> {
    try_run(s, sql)
        .await
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
}

pub async fn try_run(s: &dyn Session, sql: &str) -> Result<Vec<StatementResult>> {
    execute_collect(s, QueryRequest::new(sql)).await
}

pub fn rows_of(r: &StatementResult) -> &ResultSet {
    match &r.outcome {
        StatementOutcome::Rows(rs) => rs,
        other => panic!("expected rows, got {other:?}"),
    }
}

pub fn text(v: &Value) -> String {
    match v {
        Value::Text(s) | Value::Decimal(s) | Value::Uuid(s) | Value::Date(s) | Value::Time(s)
        | Value::DateTime(s) | Value::Timestamp(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        other => panic!("not text: {other:?}"),
    }
}

pub fn err_kind(r: Result<impl std::fmt::Debug>) -> &'static str {
    match r {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(e) => e.kind(),
    }
}

pub fn is_query_err(e: &Error) -> bool {
    matches!(e, Error::Query { .. })
}
