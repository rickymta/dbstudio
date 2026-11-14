//! Shared helpers of the ClickHouse integration tests.
//!
//! Tests only run with `DBSTUDIO_IT=1`. A server that cannot be reached makes the test print a
//! note and pass (`skip`), unless `DBSTUDIO_IT_STRICT=1` (CI) turns that into a failure.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use dbstudio_core::{
    ConnectionConfig, DbKind, Driver, QueryRequest, Result, ResultSet, Secret, Session,
    StatementOutcome, TlsConfig, TlsMode, Value, execute_collect,
};
use dbstudio_driver_clickhouse::ClickHouseDriver;
use uuid::Uuid;

pub fn it_enabled() -> bool {
    std::env::var("DBSTUDIO_IT").is_ok_and(|v| v == "1")
}

/// Strict mode for the service serving `port` (`clickhouse23` on the old-version port, else
/// `clickhouse26`).
fn strict(port: u16) -> bool {
    let service = if port == old_port() {
        "clickhouse23"
    } else {
        "clickhouse26"
    };
    dbstudio_core::testing::it_strict_for(service)
}

fn env_port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub fn http_port() -> u16 {
    env_port("DBSTUDIO_IT_CLICKHOUSE_PORT", 18123)
}

pub fn tls_port() -> u16 {
    env_port("DBSTUDIO_IT_CLICKHOUSE_TLS_PORT", 18443)
}

pub fn old_port() -> u16 {
    env_port("DBSTUDIO_IT_CLICKHOUSE23_PORT", 18023)
}

pub fn certs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../docker/certs")
}

pub fn config(port: u16) -> ConnectionConfig {
    let mut c = ConnectionConfig::new(DbKind::ClickHouse, "it", "127.0.0.1");
    c.port = port;
    c.username = Some("dbstudio".into());
    c.tls = TlsConfig {
        mode: TlsMode::Disable,
        ..Default::default()
    };
    c.connect_timeout_ms = Some(5000);
    c
}

pub fn secret() -> Secret {
    Secret::new("dbstudio_test")
}

pub async fn connect_cfg(cfg: &ConnectionConfig) -> Result<Box<dyn Session>> {
    ClickHouseDriver::new().connect(cfg, Some(&secret())).await
}

pub async fn connect_err(cfg: &ConnectionConfig) -> dbstudio_core::Error {
    connect_cfg(cfg)
        .await
        .err()
        .expect("the connection must fail")
}

/// A session on `port`, or `None` (test skipped) when integration tests are off or the server is
/// down and strict mode is not on.
pub async fn session_on(port: u16) -> Option<Arc<dyn Session>> {
    if !it_enabled() {
        return None;
    }
    match connect_cfg(&config(port)).await {
        Ok(s) => Some(Arc::from(s)),
        Err(e) => {
            if strict(port) {
                panic!("ClickHouse on port {port} is required (DBSTUDIO_IT_STRICT=1): {e}");
            }
            eprintln!("skip: ClickHouse on port {port} is not reachable: {e}");
            None
        }
    }
}

pub async fn session() -> Option<Arc<dyn Session>> {
    session_on(http_port()).await
}

pub async fn run(s: &dyn Session, sql: &str) -> Vec<dbstudio_core::StatementResult> {
    execute_collect(s, QueryRequest::new(sql))
        .await
        .unwrap_or_else(|e| panic!("sql failed: {e}\n{sql}"))
}

pub fn rows_of(r: &dbstudio_core::StatementResult) -> &ResultSet {
    match &r.outcome {
        StatementOutcome::Rows(rs) => rs,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Rows of one statement.
pub async fn query(s: &dyn Session, sql: &str) -> Vec<Vec<Value>> {
    let r = run(s, sql).await;
    rows_of(&r[0]).rows.clone()
}

pub async fn scalar(s: &dyn Session, sql: &str) -> Value {
    query(s, sql).await[0][0].clone()
}

/// A database dropped (also when the test panics) when the guard goes out of scope.
pub struct TempDb {
    pub name: String,
    port: u16,
}

impl TempDb {
    pub async fn create(s: &dyn Session, port: u16, tag: &str) -> TempDb {
        let name = format!(
            "dbstudio_it_{tag}_{}",
            &Uuid::new_v4().simple().to_string()[..8]
        );
        run(s, &format!("CREATE DATABASE `{name}`")).await;
        TempDb { name, port }
    }

    pub fn t(&self, table: &str) -> String {
        format!("`{}`.`{table}`", self.name)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let (name, port) = (self.name.clone(), self.port);
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async {
                if let Ok(s) = connect_cfg(&config(port)).await {
                    let _ = execute_collect(
                        s.as_ref(),
                        QueryRequest::new(format!("DROP DATABASE IF EXISTS `{name}` SYNC")),
                    )
                    .await;
                    let _ = s.close().await;
                }
            });
        })
        .join();
    }
}
