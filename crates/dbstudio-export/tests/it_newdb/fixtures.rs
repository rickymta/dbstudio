//! Servers and fixtures of the "new database" integration matrix (export / import / backup on
//! ScyllaDB, Cassandra, ClickHouse, CockroachDB, YugabyteDB and TiDB). Shared with the backup
//! crate through `#[path]`.
#![allow(dead_code, clippy::format_in_format_args)]

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dbstudio_core::{
    ConnectionConfig, DbKind, Driver, ObjectKind, ObjectPath, QueryRequest, Secret, Session,
    StatementOutcome, TlsConfig, TlsMode, Value, execute_collect,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum E {
    ClickHouse,
    Scylla,
    Cassandra,
    Cockroach,
    Yugabyte,
    TiDb,
}

impl E {
    pub fn kind(self) -> DbKind {
        match self {
            E::ClickHouse => DbKind::ClickHouse,
            E::Scylla => DbKind::ScyllaDb,
            E::Cassandra => DbKind::Cassandra,
            E::Cockroach => DbKind::CockroachDb,
            E::Yugabyte => DbKind::YugabyteDb,
            E::TiDb => DbKind::TiDb,
        }
    }

    /// Name of the service in `docker/compose.test.yml` (what `DBSTUDIO_IT_STRICT` lists).
    pub fn service(self) -> &'static str {
        match self {
            E::ClickHouse => "clickhouse26",
            other => other.name(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            E::ClickHouse => "clickhouse",
            E::Scylla => "scylla",
            E::Cassandra => "cassandra5",
            E::Cockroach => "cockroach",
            E::Yugabyte => "yugabyte",
            E::TiDb => "tidb",
        }
    }

    pub fn port(self) -> u16 {
        let (var, default) = match self {
            E::ClickHouse => ("DBSTUDIO_IT_CLICKHOUSE_PORT", 18123),
            E::Scylla => ("DBSTUDIO_IT_SCYLLA_PORT", 19060),
            E::Cassandra => ("DBSTUDIO_IT_CASSANDRA5_PORT", 19050),
            E::Cockroach => ("DBSTUDIO_IT_COCKROACH_PORT", 26258),
            E::Yugabyte => ("DBSTUDIO_IT_YUGABYTE_PORT", 54333),
            E::TiDb => ("DBSTUDIO_IT_TIDB_PORT", 34000),
        };
        std::env::var(var)
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(default)
    }

    pub fn is_cql(self) -> bool {
        matches!(self, E::Scylla | E::Cassandra)
    }

    /// PostgreSQL protocol: one connection per database, schemas inside.
    pub fn is_pg(self) -> bool {
        matches!(self, E::Cockroach | E::Yugabyte)
    }
}

pub fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").is_ok_and(|v| v == "1")
}

/// `true` when the test should run on `e` (integration tests on and the server reachable).
pub fn probe(e: E) -> bool {
    if !enabled() {
        return false;
    }
    let up = ("127.0.0.1", e.port())
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_millis(500)).is_ok());
    if !up {
        assert!(
            !dbstudio_core::testing::it_strict_for(e.service()),
            "{} is not reachable (DBSTUDIO_IT_STRICT)",
            e.name()
        );
        eprintln!("skip: {} is not reachable on port {}", e.name(), e.port());
    }
    up
}

fn certs() -> String {
    format!(
        "{}/../../docker/certs/cockroach",
        env!("CARGO_MANIFEST_DIR")
    )
}

pub async fn run(s: &dyn Session, database: Option<&str>, sql: &str) {
    let mut req = QueryRequest::new(sql);
    req.database = database.map(str::to_string);
    if let Err(e) = execute_collect(s, req).await {
        panic!("sql failed: {e}\n{}", &sql[..sql.len().min(500)]);
    }
}

pub async fn try_run(s: &dyn Session, database: Option<&str>, sql: &str) {
    let mut req = QueryRequest::new(sql);
    req.database = database.map(str::to_string);
    let _ = execute_collect(s, req).await;
}

/// Map entries have no order: sort document fields (a JSON round trip may reorder them).
fn norm(v: Value) -> Value {
    match v {
        Value::Document(mut f) => {
            f.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Document(f.into_iter().map(|(k, v)| (k, norm(v))).collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(norm).collect()),
        other => other,
    }
}

/// Rows of a query as sorted debug strings (order-insensitive comparison).
pub async fn rows_of(s: &dyn Session, database: Option<&str>, sql: &str) -> Vec<String> {
    let mut req = QueryRequest::new(sql);
    req.database = database.map(str::to_string);
    let res = execute_collect(s, req)
        .await
        .unwrap_or_else(|e| panic!("query failed: {e}\n{sql}"));
    let mut out: Vec<String> = match &res[0].outcome {
        StatementOutcome::Rows(rs) => rs
            .rows
            .iter()
            .map(|r| format!("{:?}", r.iter().cloned().map(norm).collect::<Vec<_>>()))
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    };
    out.sort();
    out
}

pub async fn scalar(s: &dyn Session, database: Option<&str>, sql: &str) -> String {
    let mut req = QueryRequest::new(sql);
    req.database = database.map(str::to_string);
    let res = execute_collect(s, req)
        .await
        .unwrap_or_else(|e| panic!("query failed: {e}\n{sql}"));
    match &res[0].outcome {
        StatementOutcome::Rows(rs) => match &rs.rows[0][0] {
            Value::Text(t) | Value::Decimal(t) => t.clone(),
            Value::Int(i) => i.to_string(),
            Value::UInt(i) => i.to_string(),
            other => format!("{other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

async fn admin_session(e: E) -> Arc<dyn Session> {
    let mut cfg = ConnectionConfig::new(e.kind(), "it", "127.0.0.1");
    cfg.port = e.port();
    cfg.connect_timeout_ms = Some(30_000);
    let s: Box<dyn Session> = match e {
        E::ClickHouse => {
            cfg.username = Some("dbstudio".into());
            cfg.tls.mode = TlsMode::Disable;
            dbstudio_driver_clickhouse::ClickHouseDriver::new()
                .connect(&cfg, Some(&Secret::new("dbstudio_test")))
                .await
        }
        E::TiDb => {
            cfg.username = Some("root".into());
            cfg.tls.mode = TlsMode::Disable;
            dbstudio_driver_mysql::TiDbDriver::new()
                .connect(&cfg, None)
                .await
        }
        E::Scylla | E::Cassandra => {
            cfg.username = Some("cassandra".into());
            cfg.tls.mode = TlsMode::Disable;
            cfg.options.insert("cql.singleNode".into(), "true".into());
            let secret = Secret::new("cassandra");
            if e == E::Scylla {
                dbstudio_driver_cassandra::ScyllaDriver::new()
                    .connect(&cfg, Some(&secret))
                    .await
            } else {
                dbstudio_driver_cassandra::CassandraDriver::new()
                    .connect(&cfg, Some(&secret))
                    .await
            }
        }
        E::Cockroach => {
            cfg.username = Some("root".into());
            cfg.database = Some("defaultdb".into());
            cfg.tls = TlsConfig {
                mode: TlsMode::VerifyFull,
                ca_cert_path: Some(format!("{}/ca.crt", certs())),
                client_cert_path: Some(format!("{}/client.root.crt", certs())),
                client_key_path: Some(format!("{}/client.root.key", certs())),
                server_name: None,
            };
            dbstudio_driver_postgres::PostgresFlavorDriver::new(e.kind())
                .connect(&cfg, None)
                .await
        }
        E::Yugabyte => {
            cfg.username = Some("yugabyte".into());
            cfg.database = Some("yugabyte".into());
            cfg.tls.mode = TlsMode::Prefer;
            dbstudio_driver_postgres::PostgresFlavorDriver::new(e.kind())
                .connect(&cfg, Some(&Secret::new("yugabyte")))
                .await
        }
    }
    .unwrap_or_else(|err| panic!("[{}] connect failed: {err}", e.name()));
    Arc::from(s)
}

/// A session on database `db` of a PostgreSQL-protocol server (root / yugabyte).
async fn pg_session(e: E, db: &str) -> Arc<dyn Session> {
    let mut cfg = ConnectionConfig::new(e.kind(), "it", "127.0.0.1");
    cfg.port = e.port();
    cfg.connect_timeout_ms = Some(30_000);
    cfg.database = Some(db.into());
    let secret = if e == E::Cockroach {
        cfg.username = Some("root".into());
        cfg.tls = TlsConfig {
            mode: TlsMode::VerifyFull,
            ca_cert_path: Some(format!("{}/ca.crt", certs())),
            client_cert_path: Some(format!("{}/client.root.crt", certs())),
            client_key_path: Some(format!("{}/client.root.key", certs())),
            server_name: None,
        };
        None
    } else {
        cfg.username = Some("yugabyte".into());
        cfg.tls.mode = TlsMode::Prefer;
        Some(Secret::new("yugabyte"))
    };
    let s = dbstudio_driver_postgres::PostgresFlavorDriver::new(e.kind())
        .connect(&cfg, secret.as_ref())
        .await
        .unwrap_or_else(|err| panic!("[{}] connect to {db}: {err}", e.name()));
    Arc::from(s)
}

/// An empty namespace (database / keyspace) with a session that works in it.
pub struct Dst {
    pub session: Arc<dyn Session>,
    pub ns: String,
    /// Value for `ImportTarget::Script { database }` and `QueryRequest::database`.
    pub script_db: Option<String>,
    /// Qualifier prefix for statements (`"ns."` or empty on PostgreSQL protocol servers).
    pub q: String,
}

/// A server with the fixture loaded into `src_ns`.
pub struct Fx {
    pub e: E,
    pub label: String,
    pub admin: Arc<dyn Session>,
    pub src: Dst,
    created: Mutex<Vec<String>>,
}

impl Drop for Fx {
    fn drop(&mut self) {
        let dbs = std::mem::take(&mut *self.created.lock().unwrap_or_else(|e| e.into_inner()));
        let e = self.e;
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                let admin = admin_session(e).await;
                for db in dbs {
                    drop_ns(e, &*admin, &db).await;
                }
                let _ = admin.close().await;
            });
        })
        .join();
    }
}

async fn drop_ns(e: E, admin: &dyn Session, db: &str) {
    match e {
        E::Scylla | E::Cassandra => {
            try_run(admin, None, &format!("DROP KEYSPACE IF EXISTS {db}")).await
        }
        E::Cockroach | E::Yugabyte => {
            if e == E::Yugabyte {
                try_run(
                    admin,
                    None,
                    &format!(
                        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db}' AND pid <> pg_backend_pid()"
                    ),
                )
                .await;
            }
            try_run(
                admin,
                None,
                &format!("DROP DATABASE IF EXISTS {db} CASCADE"),
            )
            .await;
        }
        _ => try_run(admin, None, &format!("DROP DATABASE IF EXISTS {db}")).await,
    }
}

fn tag() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

impl Fx {
    /// Connect, create a fresh source namespace and load the fixture.
    pub async fn new(e: E, label: &str) -> Fx {
        let admin = admin_session(e).await;
        if e.is_pg() {
            // The test login of the flavour suite is not needed: root / yugabyte can do everything.
        }
        let mut fx = Fx {
            e,
            label: label.to_string(),
            admin: Arc::clone(&admin),
            src: Dst {
                session: Arc::clone(&admin),
                ns: String::new(),
                script_db: None,
                q: String::new(),
            },
            created: Mutex::new(Vec::new()),
        };
        fx.src = fx.fresh("src").await;
        fx.load(&fx.src).await;
        fx
    }

    /// Create an empty namespace.
    pub async fn fresh(&self, what: &str) -> Dst {
        let ns = format!("exp_it_{}_{what}_{}", self.label.replace('-', "_"), tag());
        self.created.lock().unwrap().push(ns.clone());
        let e = self.e;
        match e {
            E::Scylla | E::Cassandra => {
                let tablets = if e == E::Scylla {
                    " AND tablets = {'enabled': false}"
                } else {
                    ""
                };
                run(
                    &*self.admin,
                    None,
                    &format!(
                        "CREATE KEYSPACE {ns} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}{tablets}"
                    ),
                )
                .await;
                Dst {
                    session: Arc::clone(&self.admin),
                    script_db: Some(ns.clone()),
                    q: format!("{ns}."),
                    ns,
                }
            }
            E::ClickHouse | E::TiDb => {
                run(&*self.admin, None, &format!("CREATE DATABASE {ns}")).await;
                Dst {
                    session: Arc::clone(&self.admin),
                    script_db: Some(ns.clone()),
                    q: format!("{ns}."),
                    ns,
                }
            }
            E::Cockroach | E::Yugabyte => {
                run(&*self.admin, None, &format!("CREATE DATABASE {ns}")).await;
                Dst {
                    session: pg_session(e, &ns).await,
                    script_db: None,
                    q: String::new(),
                    ns,
                }
            }
        }
    }

    /// Start dropping `ns` (a namespace a test created itself) when the fixture is dropped.
    pub fn track(&self, ns: &str) {
        self.created.lock().unwrap().push(ns.to_string());
    }

    /// Session handle on an existing namespace (and drop it with the fixture).
    pub async fn attach(&self, ns: &str) -> Dst {
        self.track(ns);
        match self.e {
            E::Cockroach | E::Yugabyte => Dst {
                session: pg_session(self.e, ns).await,
                ns: ns.to_string(),
                script_db: None,
                q: String::new(),
            },
            _ => Dst {
                session: Arc::clone(&self.admin),
                ns: ns.to_string(),
                script_db: Some(ns.to_string()),
                q: format!("{ns}."),
            },
        }
    }

    /// Connection profile and secret of the admin login (what native tools and the backup
    /// context need).
    pub fn config(&self) -> (ConnectionConfig, Option<Secret>) {
        let e = self.e;
        let mut cfg = ConnectionConfig::new(e.kind(), "it", "127.0.0.1");
        cfg.port = e.port();
        cfg.connect_timeout_ms = Some(30_000);
        match e {
            E::ClickHouse => {
                cfg.username = Some("dbstudio".into());
                (cfg, Some(Secret::new("dbstudio_test")))
            }
            E::TiDb => {
                cfg.username = Some("root".into());
                (cfg, None)
            }
            E::Scylla | E::Cassandra => {
                cfg.username = Some("cassandra".into());
                (cfg, Some(Secret::new("cassandra")))
            }
            E::Cockroach => {
                cfg.username = Some("root".into());
                cfg.database = Some("defaultdb".into());
                cfg.tls = TlsConfig {
                    mode: TlsMode::VerifyFull,
                    ca_cert_path: Some(format!("{}/ca.crt", certs())),
                    client_cert_path: Some(format!("{}/client.root.crt", certs())),
                    client_key_path: Some(format!("{}/client.root.key", certs())),
                    server_name: None,
                };
                (cfg, None)
            }
            E::Yugabyte => {
                cfg.username = Some("yugabyte".into());
                cfg.database = Some("yugabyte".into());
                (cfg, Some(Secret::new("yugabyte")))
            }
        }
    }

    /// Schema node the fixture tables live in.
    pub fn scope(&self, d: &Dst) -> ObjectPath {
        let db = ObjectPath::root().child(ObjectKind::Database, &d.ns);
        if self.e.is_pg() {
            db.child(ObjectKind::Folder, "schemas")
                .child(ObjectKind::Schema, "public")
        } else {
            db
        }
    }

    pub fn table(&self, d: &Dst, name: &str) -> ObjectPath {
        self.scope(d)
            .child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, name)
    }

    pub fn source(&self, d: &Dst) -> dbstudio_export::ExportSource {
        let path = self.scope(d);
        if self.e.is_pg() {
            dbstudio_export::ExportSource::Schema { path }
        } else {
            dbstudio_export::ExportSource::Database { path }
        }
    }

    /// Fixture tables: `(name, rows)`.
    pub fn tables(&self) -> Vec<(&'static str, u64)> {
        match self.e {
            E::ClickHouse => vec![("exp_ch", ROWS), ("exp_ch_log", 40)],
            E::Scylla | E::Cassandra => vec![("exp_cql", CQL_ROWS), ("exp_cql_ts", 30)],
            E::Cockroach | E::Yugabyte => vec![("exp_pg", ROWS), ("exp_pg_child", 30)],
            E::TiDb => vec![("exp_my", ROWS), ("exp_my_child", 30)],
        }
    }

    pub fn total_rows(&self) -> u64 {
        self.tables().iter().map(|t| t.1).sum()
    }

    /// Order-insensitive content of every fixture table in `d`.
    pub async fn contents(&self, d: &Dst) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        for (t, _) in self.tables() {
            out.push((t.to_string(), self.content(d, t).await));
        }
        out
    }

    pub async fn content(&self, d: &Dst, table: &str) -> Vec<String> {
        rows_of(
            &*d.session,
            d.script_db.as_deref(),
            &format!("SELECT * FROM {}{table}", d.q),
        )
        .await
    }

    /// Run the fixture DDL (no rows) in `d`.
    pub async fn create_schema(&self, d: &Dst) {
        for sql in schema_sql(self.e) {
            run(
                &*d.session,
                d.script_db.as_deref(),
                &sql.replace("{q}", &d.q),
            )
            .await;
        }
    }

    async fn load(&self, d: &Dst) {
        self.create_schema(d).await;
        for sql in data_sql(self.e) {
            run(
                &*d.session,
                d.script_db.as_deref(),
                &sql.replace("{q}", &d.q),
            )
            .await;
        }
    }
}

pub const ROWS: u64 = 300;
pub const CQL_ROWS: u64 = 120;

/// DDL of the fixture, `{q}` is the namespace qualifier.
pub fn schema_sql(e: E) -> Vec<String> {
    match e {
        E::ClickHouse => vec![
            "CREATE TABLE {q}exp_ch (
              id UInt64, name String, note Nullable(String), tag LowCardinality(String),
              lcn LowCardinality(Nullable(String)), u8 UInt8, i64 Int64, f32 Float32, f64 Float64,
              dec Decimal(18,4), dec2 Decimal(38,10), flag Bool, d Date, d32 Date32,
              dt DateTime('UTC'), dt64 DateTime64(3,'UTC'), dt64u DateTime64(6,'UTC'),
              uid UUID, ip4 IPv4, ip6 IPv6, en Enum8('red' = 1, 'green' = 2), fs FixedString(3),
              arr Array(Int32), arrn Array(Nullable(String)), arr2 Array(Array(Int32)),
              mp Map(String, Int32), tp Tuple(Int32, String), big UInt128, bi Int256,
              nul Nullable(Int32), dtn Nullable(DateTime64(3,'UTC'))
            ) ENGINE = MergeTree ORDER BY id COMMENT 'matrix table'"
                .into(),
            "CREATE TABLE {q}exp_ch_log (ts DateTime64(3,'UTC'), level LowCardinality(String), msg String, attrs Map(String, String))
             ENGINE = MergeTree PARTITION BY toYYYYMM(ts) ORDER BY ts"
                .into(),
        ],
        E::Scylla | E::Cassandra => vec![
            "CREATE TYPE {q}addr (street text, zip int, geo frozen<list<double>>)".into(),
            "CREATE TABLE {q}exp_cql (
              id bigint PRIMARY KEY, name text, note text, flag boolean, i8 tinyint, i16 smallint,
              i32 int, f float, d double, dec decimal, vi varint, dt date, tm time, ts timestamp,
              uid uuid, tuid timeuuid, ip inet, blb blob, dur duration, asc_c ascii,
              lst list<int>, st set<text>, mp map<text,int>, ud frozen<addr>,
              tp frozen<tuple<int,text>>, nested map<text, frozen<list<int>>>,
              ulst list<frozen<addr>>, nul int
            ) WITH comment = 'matrix table'"
                .into(),
            "CREATE TABLE {q}exp_cql_ts (pk int, ck int, v text, s text static, PRIMARY KEY (pk, ck))
             WITH CLUSTERING ORDER BY (ck DESC)"
                .into(),
        ],
        E::Cockroach | E::Yugabyte => vec![
            "CREATE TABLE exp_pg (
              id INT8 PRIMARY KEY, name TEXT, qty INT4 NOT NULL DEFAULT 0, price DECIMAL(12,2),
              ratio FLOAT8, flag BOOL, born DATE, created TIMESTAMP, seen TIMESTAMPTZ,
              blob BYTEA, doc JSONB, uid UUID, tags TEXT[], nums INT8[], ip INET, iv INTERVAL,
              small INT2, r4 FLOAT4
            )"
            .into(),
            "CREATE TABLE exp_pg_child (id INT4 PRIMARY KEY, a_id INT8 NOT NULL REFERENCES exp_pg (id), note TEXT)"
                .into(),
            "CREATE SEQUENCE exp_seq START 100".into(),
        ],
        E::TiDb => vec![
            "CREATE TABLE {q}exp_my (
              id BIGINT PRIMARY KEY, name TEXT, qty INT NOT NULL DEFAULT 0, price DECIMAL(12,2),
              ratio DOUBLE, flag TINYINT(1), born DATE, created DATETIME(3), seen TIMESTAMP,
              tm TIME(3), yr YEAR, bin BLOB, doc JSON, en ENUM('a','b','c'), st SET('x','y','z'),
              u64 BIGINT UNSIGNED, bits BIT(8), f32 FLOAT, KEY exp_my_q (qty)
            ) COMMENT='main table'"
                .into(),
            "CREATE TABLE {q}exp_my_child (id INT PRIMARY KEY, a_id BIGINT NOT NULL, note TEXT, KEY exp_my_child_a (a_id))"
                .into(),
        ],
    }
}

fn cql_text(i: u64) -> String {
    // Quotes, backslash, newline, unicode, an emoji.
    format!("row {i} ''q'' \\ s\nline ü漢 \u{1F600}")
}

/// Rows of the fixture.
pub fn data_sql(e: E) -> Vec<String> {
    match e {
        E::ClickHouse => vec![
            "INSERT INTO {q}exp_ch SELECT
              number + 1, if(number % 7 = 0, ' ', concat('row ', toString(number), ' \"q\" \\\\ \\'s\\nline ü漢 😀')),
              if(number % 5 = 0, NULL, concat('n', toString(number))), concat('t', toString(number % 3)),
              if(number % 4 = 0, NULL, 'z'), toUInt8(number % 256), toInt64(number) * -1000003,
              toFloat32(number) / 8, number / 7.0,
              toDecimal64(number * 1.25, 4), toDecimal128(concat(toString(number), '.1234567891'), 10),
              number % 2 = 0, toDate('2020-01-01') + number % 365, toDate32('1960-01-01') + number,
              toDateTime('2020-01-01 00:00:00', 'UTC') + number,
              toDateTime64('2020-01-01 00:00:00.123', 3, 'UTC') + number,
              toDateTime64('2020-01-01 00:00:00.123456', 6, 'UTC') + number,
              toUUID(concat('00000000-0000-4000-8000-', leftPad(toString(number), 12, '0'))),
              toIPv4(concat('10.0.', toString(number % 256), '.1')), toIPv6('2001:db8::1'),
              if(number % 2 = 0, 'red', 'green'), toFixedString('abc', 3),
              [toInt32(number), toInt32(number) * 2], ['a', NULL, toString(number)],
              [[1, 2], [toInt32(number)], []], map('k1', toInt32(number), 'k\"2', 2),
              tuple(toInt32(number), 'x,y'), toUInt128(concat(toString(number), '00000000000000000000')),
              toInt256(concat('-', toString(number), '00000000000000000000000000000000000000')),
              if(number % 3 = 0, NULL, toInt32(number)),
              if(number % 3 = 0, NULL, toDateTime64('2021-02-03 04:05:06.789', 3, 'UTC'))
             FROM numbers(300)"
                .into(),
            "INSERT INTO {q}exp_ch_log SELECT toDateTime64('2023-01-15 00:00:00.001', 3, 'UTC') + number * 86400 * 20,
              if(number % 2 = 0, 'info', 'warn'), concat('message ', toString(number)), map('n', toString(number), 'q\\'', 'v')
             FROM numbers(40)"
                .into(),
        ],
        E::Scylla | E::Cassandra => {
            let mut v = Vec::new();
            for i in 1..=CQL_ROWS {
                let text = cql_text(i);
                let note = if i % 5 == 0 {
                    "null".to_string()
                } else {
                    format!("'n{i}'")
                };
                let nul = if i % 3 == 0 {
                    "null".to_string()
                } else {
                    i.to_string()
                };
                let lst = if i % 4 == 0 {
                    "[]".to_string()
                } else {
                    format!("[{i}, {}, 3]", i * 2)
                };
                v.push(format!(
                    "INSERT INTO {{q}}exp_cql (id, name, note, flag, i8, i16, i32, f, d, dec, vi, dt, tm, ts, uid, tuid, ip, blb, dur, asc_c, lst, st, mp, ud, tp, nested, ulst, nul) VALUES (
                      {i}, '{text}', {note}, {flag}, {i8}, {i16}, {i32}, {f}, {d}, {dec}, {vi}, '{dt}', '{tm}', '{ts}',
                      {uid}, {tuid}, '{ip}', 0x{blb}, {dur}, 'asc{i}', {lst}, {{'b{i}', 'a', 'c d'}}, {{'k{i}': {i}, 'z': -1}},
                      {{street: 'S {i}', zip: {i}, geo: [1.5, {i}.25]}}, ({i}, 'tuple, text'), {{'x': [1, {i}], 'y': []}},
                      [{{street: 'A', zip: 1, geo: null}}, {{street: 'B''s', zip: 2, geo: [0.5]}}], {nul})",
                    flag = i % 2 == 0,
                    i8 = (i % 100) as i64 - 50,
                    i16 = i * 100,
                    i32 = i * 1_000_003,
                    f = format!("{}.5", i),
                    d = format!("{}.123456789", i),
                    dec = format!("{i}12345678901234567890.123456789"),
                    vi = format!("{i}000000000000000000000000000001"),
                    dt = format!("2020-{:02}-{:02}", 1 + i % 12, 1 + i % 28),
                    tm = format!("{:02}:{:02}:{:02}.123456789", i % 24, i % 60, i % 60),
                    ts = format!("2021-05-{:02}T10:20:30.123Z", 1 + i % 28),
                    uid = format!("00000000-0000-4000-8000-{:012}", i),
                    tuid = format!("{:08x}-0000-1000-8000-000000000001", i),
                    ip = if i % 2 == 0 { format!("10.0.0.{}", i % 250) } else { format!("2001:db8::{i:x}") },
                    blb = format!("{:08x}", i * 7919),
                    dur = ["1h30m", "P1Y2M3DT4H", "5d", "-3m", "12us"][(i % 5) as usize],
                ));
            }
            for pk in 1..=3 {
                for ck in 1..=10 {
                    v.push(format!(
                        "INSERT INTO {{q}}exp_cql_ts (pk, ck, v, s) VALUES ({pk}, {ck}, 'v{pk}-{ck}', 's{pk}')"
                    ));
                }
            }
            v
        }
        E::Cockroach | E::Yugabyte => vec![
            "INSERT INTO exp_pg SELECT g,
              CASE WHEN g % 7 = 0 THEN NULL ELSE 'row ' || g::TEXT || E' \"q\" \\\\ ''s\\nline ü漢' END,
              (g % 100)::INT4, (g * 1.25)::DECIMAL(12,2), g::FLOAT8 / 7.0, g % 2 = 0,
              DATE '2020-01-01' + (g % 365)::INT4, TIMESTAMP '2020-01-01 00:00:00' + g * INTERVAL '1 second',
              TIMESTAMPTZ '2020-01-01 00:00:00+00' + g * INTERVAL '1 minute',
              decode(md5(g::TEXT), 'hex'), jsonb_build_object('n', g, 's', 'x'), md5(g::TEXT)::UUID,
              ARRAY['a' || g::TEXT, 'b c', NULL], ARRAY[g, g * 2, -g], ('10.0.0.' || (g % 250)::TEXT)::INET,
              (g || ' minutes')::INTERVAL, (g % 1000)::INT2, (g::FLOAT8 / 8.0)::FLOAT4
             FROM generate_series(1, 300) g"
                .into(),
            "INSERT INTO exp_pg_child SELECT g, g * 10, 'note ' || g::TEXT FROM generate_series(1, 30) g".into(),
            // Used three times: the next value of a restored copy must be 103.
            "SELECT nextval('exp_seq'), nextval('exp_seq'), nextval('exp_seq')".into(),
        ],
        E::TiDb => vec![
            "INSERT INTO {q}exp_my WITH RECURSIVE s(g) AS (SELECT 1 UNION ALL SELECT g + 1 FROM s WHERE g < 300)
             SELECT g,
               CASE WHEN g % 7 = 0 THEN NULL ELSE CONCAT('row ', g, ' \"q\" \\\\ ''s', CHAR(10), 'line ü漢 😀') END,
               g % 100, g * 1.25, g / 7.0, g % 2 = 0, DATE_ADD('2020-01-01', INTERVAL g % 365 DAY),
               TIMESTAMPADD(SECOND, g, '2020-01-01 00:00:00.123'), TIMESTAMPADD(SECOND, g, '2021-01-01 00:00:00'),
               SEC_TO_TIME(g * 7) , 1990 + g % 100, UNHEX(MD5(g)),
               CONCAT('{\"n\": ', g, ', \"s\": \"x\"}'), ELT(g % 3 + 1, 'a', 'b', 'c'),
               ELT(g % 3 + 1, 'x', 'x,y', 'x,y,z'), 18446744073709551615 - g, g % 256, g / 8
             FROM s"
                .into(),
            "INSERT INTO {q}exp_my_child WITH RECURSIVE s(g) AS (SELECT 1 UNION ALL SELECT g + 1 FROM s WHERE g < 30)
             SELECT g, g * 10, CONCAT('note ', g) FROM s"
                .into(),
        ],
    }
}
