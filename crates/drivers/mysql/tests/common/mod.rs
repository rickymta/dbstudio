//! Shared helpers of the integration tests (only active when `DBSTUDIO_IT=1`).
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use dbstudio_core::{
    CollectingSink, ConnectionConfig, DbKind, Driver, Error, QueryRequest, Result, ResultSet,
    ResultSink, Secret, Session, StatementOutcome, TableDataRequest, TlsConfig, TlsMode, Value,
    execute_collect,
};
use dbstudio_driver_mysql::MySqlDriver;
use tokio::sync::OnceCell;
use uuid::Uuid;

pub const FIXTURE_DB: &str = "dbstudio_it_fx";

#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub name: &'static str,
    pub port: u16,
    pub mariadb: bool,
    pub tls: bool,
    /// Expected `server_info().version` prefix.
    pub version: &'static str,
    /// Emulated / slow server: tests allow more time.
    pub slow: bool,
    /// TiDB (MySQL protocol, `root` without a password, no procedures / triggers / events).
    pub tidb: bool,
}

pub const MYSQL8: Target = Target {
    name: "mysql8",
    port: 33068,
    mariadb: false,
    tls: true,
    version: "8.4",
    slow: false,
    tidb: false,
};
pub const MARIADB: Target = Target {
    name: "mariadb",
    port: 33106,
    mariadb: true,
    tls: false,
    version: "10.6",
    slow: false,
    tidb: false,
};
/// MySQL 5.7 (amd64 emulation: slow). It may generate its own self-signed certificate.
pub const MYSQL57: Target = Target {
    name: "mysql57",
    port: 33057,
    mariadb: false,
    tls: false,
    version: "5.7",
    slow: true,
    tidb: false,
};
pub const MARIADB104: Target = Target {
    name: "mariadb104",
    port: 33104,
    mariadb: true,
    tls: false,
    version: "10.4",
    slow: false,
    tidb: false,
};
/// TiDB (`newsql` profile, port `DBSTUDIO_IT_TIDB_PORT`, default 34000). Probe-then-skip: when
/// the container is not running the target is left out, unless `DBSTUDIO_IT_STRICT` names `tidb`.
pub const TIDB: Target = Target {
    name: "tidb",
    port: 34000,
    mariadb: false,
    tls: false,
    version: "8.0.11-TiDB",
    slow: true,
    tidb: true,
};
pub const TARGETS: [Target; 5] = [MYSQL8, MARIADB, MYSQL57, MARIADB104, TIDB];

/// Port of `t` (TiDB's can be overridden by `DBSTUDIO_IT_TIDB_PORT`).
pub fn port_of(t: Target) -> u16 {
    if t.tidb {
        std::env::var("DBSTUDIO_IT_TIDB_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(t.port)
    } else {
        t.port
    }
}

fn reachable(t: Target) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr = SocketAddr::from(([127, 0, 0, 1], port_of(t)));
    TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(1)).is_ok()
}

/// Servers to test: those whose container runs (or that `DBSTUDIO_IT_STRICT` names, so a missing
/// one fails instead of being left out), or the comma-separated names in `DBSTUDIO_IT_SERVERS`.
pub fn targets() -> Vec<Target> {
    match std::env::var("DBSTUDIO_IT_SERVERS") {
        Ok(list) => TARGETS
            .into_iter()
            .filter(|t| list.split(',').any(|n| n.trim() == t.name))
            .collect(),
        Err(_) => TARGETS
            .into_iter()
            .filter(|t| dbstudio_core::testing::it_strict_for(t.name) || reachable(*t))
            .collect(),
    }
}

/// Whether `t` is part of this run.
pub fn selected(t: Target) -> bool {
    targets().iter().any(|x| x.name == t.name)
}

/// Scale a timing bound for slow (emulated) servers.
pub fn slack(t: Target, d: std::time::Duration) -> std::time::Duration {
    if t.slow { d * 4 } else { d }
}

pub fn it_enabled() -> bool {
    std::env::var("DBSTUDIO_IT").is_ok_and(|v| v == "1")
}

pub fn certs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../docker/certs")
}

pub fn config(t: Target, user: &str, tls: TlsMode) -> ConnectionConfig {
    let mut c = ConnectionConfig::new(DbKind::MySql, t.name, "127.0.0.1");
    c.port = port_of(t);
    c.username = Some(user.to_string());
    c.tls = TlsConfig {
        mode: tls,
        ..Default::default()
    };
    c.connect_timeout_ms = Some(if t.tidb { 20_000 } else { 5000 });
    if t.tidb {
        // TiDB's connection collation is binary (case-sensitive filters); the shared tests expect
        // the case-insensitive comparisons of MySQL, which `init_command` provides.
        c.options.insert(
            "init_command".into(),
            "SET collation_connection = 'utf8mb4_general_ci'".into(),
        );
    }
    c
}

pub fn secret() -> Secret {
    Secret::new("dbstudio_test")
}

pub async fn connect_cfg(cfg: &ConnectionConfig) -> Result<Box<dyn Session>> {
    // TiDB's `root` has no password (compose); `dbstudio` is created by `ensure_tidb_user`.
    // All grants happen before any other connection: TiDB reloads its privilege cache on every
    // GRANT and checks made during a reload are not reliable.
    let is_tidb = cfg.port == port_of(TIDB);
    if is_tidb {
        ensure_tidb_user().await;
    }
    if is_tidb && cfg.username.as_deref() == Some("root") {
        return MySqlDriver::new().connect(cfg, None).await;
    }
    MySqlDriver::new().connect(cfg, Some(&secret())).await
}

static TIDB_USER: OnceCell<()> = OnceCell::const_new();

/// Create the `dbstudio` login on TiDB once per process.
async fn ensure_tidb_user() {
    TIDB_USER
        .get_or_init(|| async {
            let mut cfg = config(TIDB, "root", TlsMode::Disable);
            cfg.database = None;
            let root = MySqlDriver::new()
                .connect(&cfg, None)
                .await
                .unwrap_or_else(|e| panic!("[tidb] root connect failed: {e}"));
            run(
                &*root,
                "CREATE USER IF NOT EXISTS 'dbstudio'@'%' IDENTIFIED BY 'dbstudio_test'; \
                 CREATE DATABASE IF NOT EXISTS dbstudio; \
                 GRANT ALL PRIVILEGES ON `dbstudio`.* TO 'dbstudio'@'%'; \
                 GRANT ALL PRIVILEGES ON `dbstudio\\_it%`.* TO 'dbstudio'@'%'",
            )
            .await;
            root.close().await.unwrap();
            // Let the privilege cache settle.
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        })
        .await;
}

/// Session as the `dbstudio` user without TLS.
pub async fn user_session(t: Target) -> Arc<dyn Session> {
    let s = connect_cfg(&config(t, "dbstudio", TlsMode::Disable))
        .await
        .unwrap_or_else(|e| panic!("[{}] connect failed: {e}", t.name));
    Arc::from(s)
}

pub async fn root_session(t: Target) -> Box<dyn Session> {
    connect_cfg(&config(t, "root", TlsMode::Disable))
        .await
        .unwrap_or_else(|e| panic!("[{}] root connect failed: {e}", t.name))
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

/// First statement's rows.
pub async fn query(s: &dyn Session, sql: &str) -> ResultSet {
    let mut r = run(s, sql).await;
    match r.remove(0).outcome {
        StatementOutcome::Rows(rs) => rs,
        other => panic!("expected rows, got {other:?}"),
    }
}

pub fn ints(rs: &ResultSet, col: usize) -> Vec<i64> {
    rs.rows
        .iter()
        .map(|r| match &r[col] {
            Value::Int(i) => *i,
            Value::UInt(u) => *u as i64,
            other => panic!("not an int: {other:?}"),
        })
        .collect()
}

pub async fn fetch(s: &dyn Session, req: TableDataRequest) -> Result<ResultSet> {
    let mut sink = CollectingSink::new();
    s.fetch_table(req, &mut sink).await?;
    let mut res = sink.into_results();
    match res.remove(0).outcome {
        StatementOutcome::Rows(rs) => Ok(rs),
        other => panic!("expected rows, got {other:?}"),
    }
}

pub fn table_req(db: &str, table: &str) -> TableDataRequest {
    TableDataRequest {
        query_id: Uuid::new_v4(),
        table: dbstudio_core::ObjectPath::root()
            .child(dbstudio_core::ObjectKind::Database, db)
            .child(dbstudio_core::ObjectKind::Table, table),
        filters: vec![],
        raw_where: None,
        sort: vec![],
        offset: 0,
        limit: 1000,
        batch_size: None,
    }
}

/// Sink that drops rows but counts batches and rows; optionally fails after N batches.
#[derive(Debug, Default)]
pub struct CountingSink {
    pub batches: usize,
    pub rows: usize,
    pub columns: usize,
    pub ends: usize,
    pub fail_after: Option<(usize, Option<String>)>,
    pub delay_ms: u64,
    /// Incremented on every batch (lets a test wait until streaming is under way).
    pub progress: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

impl ResultSink for CountingSink {
    fn on_columns(&mut self, c: Vec<dbstudio_core::ColumnMeta>) -> Result<()> {
        self.columns = c.len();
        Ok(())
    }
    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        self.batches += 1;
        if let Some(p) = &self.progress {
            p.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.rows += rows.len();
        if self.delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
        }
        if let Some((n, msg)) = &self.fail_after {
            if self.batches >= *n {
                return Err(match msg {
                    Some(m) => Error::Other(m.clone()),
                    None => Error::Cancelled,
                });
            }
        }
        Ok(())
    }
    fn on_message(&mut self, _: String) -> Result<()> {
        Ok(())
    }
    fn on_statement_end(&mut self, _: dbstudio_core::StatementEnd) -> Result<()> {
        self.ends += 1;
        Ok(())
    }
}

static FIXTURES: [OnceCell<()>; 5] = [
    OnceCell::const_new(),
    OnceCell::const_new(),
    OnceCell::const_new(),
    OnceCell::const_new(),
    OnceCell::const_new(),
];

/// Create (once per process and server) the shared read-only fixture database.
pub async fn ensure_fixture(t: Target) {
    let idx = TARGETS.iter().position(|x| x.name == t.name).unwrap();
    FIXTURES[idx]
        .get_or_init(|| async move { build_fixture(t).await })
        .await;
}

/// TiDB defaults to the binary collation; the tests expect case-insensitive comparisons like
/// MySQL / MariaDB, so its databases are created with a case-insensitive one.
pub fn ci_collation(t: Target) -> &'static str {
    if t.tidb {
        " COLLATE utf8mb4_general_ci"
    } else {
        ""
    }
}

const SCRIPT_TABLES: &[&str] = &[
    "CREATE TABLE all_types (
        id INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        c_tinyint TINYINT, c_smallint SMALLINT, c_mediumint MEDIUMINT, c_int INT, c_bigint BIGINT,
        u_tinyint TINYINT UNSIGNED, u_smallint SMALLINT UNSIGNED, u_mediumint MEDIUMINT UNSIGNED,
        u_int INT UNSIGNED, u_bigint BIGINT UNSIGNED,
        c_decimal DECIMAL(38,10), c_float FLOAT, c_double DOUBLE,
        c_date DATE NULL, c_datetime DATETIME(6) NULL, c_timestamp TIMESTAMP NULL, c_time TIME(3), c_year YEAR,
        c_char CHAR(5), c_varchar VARCHAR(50), c_text TEXT, c_binary BINARY(4), c_varbinary VARBINARY(20),
        c_blob BLOB, c_json JSON, c_enum ENUM('a','b'), c_set SET('x','y','z'), c_bit BIT(12),
        c_geom POINT NULL, c_zero_dt DATETIME NULL
     ) COMMENT='every type'",
    "CREATE TABLE people (
        id INT PRIMARY KEY, name VARCHAR(50), age INT, note VARCHAR(100) NULL, score DECIMAL(10,2)
     )",
    "INSERT INTO people VALUES
        (1,'Alice',30,NULL,10.50),(2,'Bob',25,'a%b',20.00),(3,'Carol',35,'a_b',30.25),
        (4,'Dave',25,'back\\\\slash',40),(5,'Eve',41,'it''s',50),(6,'Frank',NULL,'x',60),
        (7,'alice2',22,'ALICE',70)",
    "CREATE TABLE parent (id INT PRIMARY KEY, name VARCHAR(20) NOT NULL DEFAULT 'none' COMMENT 'the name')",
    "CREATE TABLE child (
        id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, parent_id INT NOT NULL, note VARCHAR(20),
        gen_len INT GENERATED ALWAYS AS (CHAR_LENGTH(note)) VIRTUAL,
        CONSTRAINT fk_child_parent FOREIGN KEY (parent_id) REFERENCES parent(id)
            ON DELETE CASCADE ON UPDATE RESTRICT,
        KEY idx_note (note),
        UNIQUE KEY uq_child (parent_id, note)
     )",
    "CREATE TABLE `We``ird Tbl` (`Id Col` INT PRIMARY KEY, `col``tick` VARCHAR(10), `Mixed Case` VARCHAR(10))",
    "INSERT INTO `We``ird Tbl` VALUES (1,'a','b'),(2,'c','d')",
    "CREATE TABLE MixedCase (Id INT PRIMARY KEY)",
    "CREATE VIEW v_child AS SELECT id, note FROM child",
    "CREATE TABLE digits (d INT PRIMARY KEY)",
    "INSERT INTO digits VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9)",
    "CREATE TABLE big (id INT PRIMARY KEY, grp INT NOT NULL, payload VARCHAR(40) NOT NULL)",
    "INSERT INTO big (id, grp, payload)
        SELECT n, n % 100, CONCAT('row-', n) FROM (
            SELECT a.d + b.d*10 + c.d*100 + e.d*1000 + f.d*10000 + g.d*100000 + 1 AS n
            FROM digits a, digits b, digits c, digits e, digits f, digits g
        ) x WHERE n <= 200000",
    "CREATE DEFINER='dbstudio'@'%' FUNCTION fn_add(a INT, b INT) RETURNS INT DETERMINISTIC RETURN a + b",
    "CREATE DEFINER='dbstudio'@'%' PROCEDURE pr_noop() SELECT 1",
    "CREATE TRIGGER trg_child_bi BEFORE INSERT ON child FOR EACH ROW SET NEW.note = IFNULL(NEW.note, 't')",
];

const ALL_TYPES_ROWS: &str = "SET SESSION sql_mode = '';
INSERT INTO all_types (c_tinyint, c_smallint, c_mediumint, c_int, c_bigint,
    u_tinyint, u_smallint, u_mediumint, u_int, u_bigint, c_decimal, c_float, c_double,
    c_date, c_datetime, c_timestamp, c_time, c_year, c_char, c_varchar, c_text, c_binary,
    c_varbinary, c_blob, c_json, c_enum, c_set, c_bit, c_geom, c_zero_dt)
VALUES (-128, -32768, -8388608, -2147483648, -9223372036854775808,
    255, 65535, 16777215, 4294967295, 18446744073709551615,
    -12345678901234567890123456.1234567890, 1.5, 1.2345678901234567,
    '2024-02-29', '2024-02-29 13:45:01.123456', '2024-02-29 13:45:01', '-838:59:59.000', 2024,
    'ab', 'héllo wörld 日本', 'long\\ntext', 'ab', x'00ff10', x'deadbeef',
    '{\"a\": [1, 2, {\"b\": null}]}', 'b', 'x,z', b'101010101010', ST_GeomFromText('POINT(1 2)'),
    '0000-00-00 00:00:00');
INSERT INTO all_types (id) VALUES (2);
INSERT INTO all_types (id, c_date, c_time, c_decimal, u_bigint) VALUES (3, '0000-00-00', '100:00:00', 0.0000000001, 0)";

async fn build_fixture(t: Target) {
    let root = root_session(t).await;
    run(
        &*root,
        &format!(
            "DROP DATABASE IF EXISTS {FIXTURE_DB}; CREATE DATABASE {FIXTURE_DB} CHARACTER SET utf8mb4{}",
            ci_collation(t)
        ),
    )
    .await;
    if !t.tidb {
        // (TiDB: granted once up front by `ensure_tidb_user`.)
        run(
            &*root,
            "GRANT ALL PRIVILEGES ON `dbstudio\\_it%`.* TO 'dbstudio'@'%'",
        )
        .await;
    }
    for sql in SCRIPT_TABLES {
        let mut sql = (*sql).to_string();
        if t.tidb {
            // TiDB: no stored routines / triggers and no spatial types.
            if sql.starts_with("CREATE DEFINER") || sql.starts_with("CREATE TRIGGER") {
                continue;
            }
            sql = sql.replace("c_geom POINT NULL, ", "");
        }
        let sql = sql.as_str();
        let mut r = QueryRequest::new(sql);
        r.database = Some(FIXTURE_DB.into());
        execute_collect(&*root, r)
            .await
            .unwrap_or_else(|e| panic!("[{}] fixture failed: {e}\n{sql}", t.name));
        if sql.starts_with("CREATE TABLE all_types") {
            // Rows are inserted after the table exists.
            let rows = if t.tidb {
                ALL_TYPES_ROWS
                    .replace("c_bit, c_geom, c_zero_dt", "c_bit, c_zero_dt")
                    .replace(
                        "b'101010101010', ST_GeomFromText('POINT(1 2)'),",
                        "b'101010101010',",
                    )
            } else {
                ALL_TYPES_ROWS.to_string()
            };
            let mut r = QueryRequest::new(rows);
            r.database = Some(FIXTURE_DB.into());
            execute_collect(&*root, r)
                .await
                .unwrap_or_else(|e| panic!("[{}] all_types rows failed: {e}", t.name));
        }
    }
    root.close().await.unwrap();
}

/// A private scratch database for tests that mutate data. It is dropped when the guard goes out
/// of scope, also when the test panics; a crashed run is cleaned by the `DROP .. IF EXISTS`
/// below the next time the same name is requested.
#[derive(Debug)]
pub struct ScratchDb {
    t: Target,
    name: String,
}

impl std::ops::Deref for ScratchDb {
    type Target = String;
    fn deref(&self) -> &String {
        &self.name
    }
}

impl std::fmt::Display for ScratchDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.name.fmt(f)
    }
}

impl Drop for ScratchDb {
    fn drop(&mut self) {
        let (t, db) = (self.t, self.name.clone());
        // A fresh thread and runtime: this may run while the test runtime is shutting down.
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                let Ok(root) = connect_cfg(&config(t, "root", TlsMode::Disable)).await else {
                    return;
                };
                let _ = execute_collect(
                    &*root,
                    QueryRequest::new(format!("DROP DATABASE IF EXISTS `{db}`")),
                )
                .await;
                let _ = root.close().await;
            });
        })
        .join();
    }
}

/// Guard that drops database `db` (any name) when it goes out of scope, also when the test
/// panics. Create it BEFORE the database so a failing setup is cleaned too.
pub fn drop_guard(t: Target, db: &str) -> ScratchDb {
    ScratchDb {
        t,
        name: db.to_string(),
    }
}

pub async fn scratch_db(t: Target, name: &str) -> ScratchDb {
    let db = format!("dbstudio_it_{name}");
    let root = root_session(t).await;
    run(
        &*root,
        &format!(
            "DROP DATABASE IF EXISTS `{db}`; CREATE DATABASE `{db}` CHARACTER SET utf8mb4{}",
            ci_collation(t)
        ),
    )
    .await;
    root.close().await.unwrap();
    ScratchDb { t, name: db }
}

/// `Debug`-like rendering of a connect result (sessions are not `Debug`).
pub fn describe<T>(r: &Result<T>) -> String {
    match r {
        Ok(_) => "Ok(..)".to_string(),
        Err(e) => format!("Err({e:?})"),
    }
}

/// Wait until a statement containing `marker` shows up in the server's process list, so cancel
/// tests do not depend on fixed sleeps.
pub async fn wait_running(t: Target, marker: &str) {
    let root = root_session(t).await;
    let sql = format!(
        "SELECT COUNT(*) FROM information_schema.PROCESSLIST \
         WHERE INFO LIKE '%{marker}%' AND INFO NOT LIKE '%PROCESSLIST%'"
    );
    for _ in 0..200 {
        let rs = query(&*root, &sql).await;
        if matches!(rs.rows[0][0], Value::Int(n) if n > 0) {
            root.close().await.unwrap();
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("[{}] statement `{marker}` never started running", t.name);
}

/// Unique SQL-comment marker.
pub fn marker() -> String {
    format!("m{}", Uuid::new_v4().simple())
}

/// Wait until `progress` reaches `n`.
pub async fn wait_progress(progress: &std::sync::atomic::AtomicUsize, n: usize) {
    for _ in 0..400 {
        if progress.load(std::sync::atomic::Ordering::SeqCst) >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("streaming never reached batch {n}");
}
