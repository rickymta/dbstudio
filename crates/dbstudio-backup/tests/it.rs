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
    logs: std::sync::Mutex<Vec<String>>,
}

impl JobReporter for Rep {
    fn progress(&self, p: &JobProgress) {
        let mut v = self.phases.lock().unwrap();
        if v.last() != Some(&p.phase) {
            v.push(p.phase.clone());
        }
    }
    fn log(&self, line: &str) {
        self.logs.lock().unwrap().push(line.to_string());
    }
    fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }
}

fn unique() -> String {
    Uuid::new_v4().simple().to_string()[..10].to_string()
}

fn config_at(kind: DbKind, port: u16) -> ConnectionConfig {
    let mut c = ConnectionConfig::new(kind, "it-conn", "127.0.0.1");
    c.port = port;
    c.username = Some("dbstudio".into());
    c.database = Some("dbstudio".into());
    c.tls.mode = TlsMode::Disable;
    c.connect_timeout_ms = Some(5000);
    c
}

async fn connect(kind: DbKind) -> (ConnectionConfig, Arc<dyn Session>) {
    connect_at(
        kind,
        if kind == DbKind::Postgres {
            54316
        } else {
            33068
        },
    )
    .await
}

async fn connect_at(kind: DbKind, port: u16) -> (ConnectionConfig, Arc<dyn Session>) {
    let cfg = config_at(kind, port);
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

/// Drops the databases of a test when it ends, also when it fails halfway (names are unique per
/// run, so anything left behind would pile up on the shared test servers).
struct DbGuard {
    kind: DbKind,
    port: u16,
    names: Vec<String>,
}

impl DbGuard {
    fn new<const N: usize>(kind: DbKind, port: u16, names: [&String; N]) -> Self {
        Self {
            kind,
            port,
            names: names.iter().map(|n| (*n).clone()).collect(),
        }
    }
}

impl Drop for DbGuard {
    fn drop(&mut self) {
        let (kind, port, names) = (self.kind, self.port, std::mem::take(&mut self.names));
        // A fresh thread and runtime: this may run while the test runtime is shutting down.
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                let (_, s) = connect_at(kind, port).await;
                for db in names {
                    if kind == DbKind::Postgres {
                        drop_pg(&*s, &db).await;
                    } else {
                        exec(&*s, None, &format!("DROP DATABASE IF EXISTS `{db}`")).await;
                    }
                }
            });
        })
        .join();
    }
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
        work_dir: None,
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
    let _cleanup = DbGuard::new(DbKind::Postgres, 54316, [&src, &dst]);
    exec(&*s, None, &format!("CREATE DATABASE `{src}`")).await;
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
        drop_pg(&*s, db).await;
    }

    // MySQL: mysqldump -> container -> mysql into an empty database.
    let (cfg, s) = connect(DbKind::MySql).await;
    let (src, dst) = (
        format!("dbstudio_it_nsrc_{n}"),
        format!("dbstudio_it_ndst_{n}"),
    );
    let _cleanup = DbGuard::new(DbKind::MySql, 33068, [&src, &dst]);
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

// ---------------------------------------------------------------------------------------------
// Full round trips: functions, procedures, triggers, matviews, sequences, partitions...
// ---------------------------------------------------------------------------------------------

const PG_FULL: &[&str] = &[
    "CREATE SCHEMA app",
    "CREATE EXTENSION IF NOT EXISTS citext",
    "CREATE TYPE app.mood AS ENUM ('sad', 'ok', 'happy')",
    "CREATE SEQUENCE app.ticket_seq START 100",
    "CREATE FUNCTION app.double(x int) RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT x * 2 $$",
    "CREATE TABLE app.people (id serial PRIMARY KEY, name text NOT NULL, mood app.mood DEFAULT 'ok', \
     tags text[], meta jsonb, photo bytea, born timestamptz, score numeric(10,2), \
     twice int GENERATED ALWAYS AS (id * 2) STORED, email public.citext, partner_id int)",
    "ALTER TABLE app.people ADD CONSTRAINT people_partner FOREIGN KEY (partner_id) REFERENCES app.people(id)",
    "CREATE TABLE app.pets (id int GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, owner_id int, name text, \
     CONSTRAINT pets_owner FOREIGN KEY (owner_id) REFERENCES app.people(id))",
    "CREATE TABLE app.a (id int PRIMARY KEY, b_id int)",
    "CREATE TABLE app.b (id int PRIMARY KEY, a_id int)",
    "ALTER TABLE app.a ADD CONSTRAINT a_b FOREIGN KEY (b_id) REFERENCES app.b(id)",
    "ALTER TABLE app.b ADD CONSTRAINT b_a FOREIGN KEY (a_id) REFERENCES app.a(id)",
    "CREATE TABLE app.events (id bigserial, at timestamptz NOT NULL, kind text, PRIMARY KEY (id, at)) \
     PARTITION BY RANGE (at)",
    "CREATE TABLE app.events_2025 PARTITION OF app.events FOR VALUES FROM ('2025-01-01') TO ('2026-01-01')",
    "CREATE TABLE app.events_2026 PARTITION OF app.events FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')",
    "CREATE TABLE app.audit (id serial PRIMARY KEY, what text)",
    "CREATE FUNCTION app.log_person() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
     INSERT INTO app.audit(what) VALUES ('person ' || NEW.name); RETURN NEW; END $$",
    "CREATE TRIGGER people_audit AFTER INSERT ON app.people FOR EACH ROW EXECUTE FUNCTION app.log_person()",
    "CREATE PROCEDURE app.add_person(n text) LANGUAGE sql AS $$ INSERT INTO app.people(name) VALUES (n) $$",
    "CREATE VIEW app.people_doubled AS SELECT id, name, app.double(id) AS d FROM app.people",
    "CREATE MATERIALIZED VIEW app.mood_stats AS SELECT mood, count(*) AS n FROM app.people GROUP BY mood",
    "CREATE TABLE app.checked (id int, qty int)",
    "INSERT INTO app.people (name, mood, tags, meta, photo, born, score, email) VALUES \
     ('Ann ''q''; \"dq\" \\ b', 'happy', ARRAY['x','y z','q\"r'], '{\"k\": [1, 2]}', '\\xdeadbeef', \
      '2026-03-04 05:06:07+00', 12.50, 'A@X.com'), \
     ('Bé Ünï 日本', 'sad', NULL, NULL, NULL, NULL, NULL, NULL), \
     ('Line\nbreak', 'ok', ARRAY[]::text[], 'null', '\\x', NULL, -0.01, NULL)",
    "UPDATE app.people SET partner_id = 1 WHERE id = 2",
    "INSERT INTO app.pets (owner_id, name) VALUES (1, 'Rex'), (2, 'Tom')",
    "INSERT INTO app.b VALUES (1, NULL)",
    "INSERT INTO app.a VALUES (1, 1)",
    "UPDATE app.b SET a_id = 1",
    "INSERT INTO app.events (at, kind) SELECT '2025-06-01'::timestamptz + (g || ' days')::interval, 'k' || g \
     FROM generate_series(0, 400) g",
    "INSERT INTO app.checked VALUES (1, 5), (2, -3)",
    "ALTER TABLE app.checked ADD CONSTRAINT qty_pos CHECK (qty > 0) NOT VALID",
    "REFRESH MATERIALIZED VIEW app.mood_stats",
    "SELECT nextval('app.ticket_seq')",
    "SELECT nextval('app.ticket_seq')",
    "SELECT nextval('app.ticket_seq')",
];

const MY_FULL: &[&str] = &[
    "CREATE TABLE people (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100) NOT NULL, \
     mood ENUM('sad','ok','happy') DEFAULT 'ok', meta JSON, photo BLOB, born TIMESTAMP NULL, \
     score DECIMAL(10,2), twice INT GENERATED ALWAYS AS (CHAR_LENGTH(name)) STORED, partner_id INT, \
     KEY people_partner_ix (partner_id), \
     CONSTRAINT people_partner FOREIGN KEY (partner_id) REFERENCES people(id))",
    "CREATE TABLE pets (id INT AUTO_INCREMENT PRIMARY KEY, owner_id INT, name VARCHAR(50), \
     CONSTRAINT pets_owner FOREIGN KEY (owner_id) REFERENCES people(id))",
    "CREATE TABLE a (id INT PRIMARY KEY, b_id INT)",
    "CREATE TABLE b (id INT PRIMARY KEY, a_id INT)",
    "ALTER TABLE a ADD CONSTRAINT a_b FOREIGN KEY (b_id) REFERENCES b(id)",
    "ALTER TABLE b ADD CONSTRAINT b_a FOREIGN KEY (a_id) REFERENCES a(id)",
    "CREATE TABLE audit (id INT AUTO_INCREMENT PRIMARY KEY, what VARCHAR(200))",
    "CREATE FUNCTION dbl(x INT) RETURNS INT DETERMINISTIC RETURN x * 2",
    "CREATE TRIGGER people_audit AFTER INSERT ON people FOR EACH ROW INSERT INTO audit(what) VALUES (CONCAT('person ', NEW.name))",
    "CREATE PROCEDURE add_person(n VARCHAR(100)) INSERT INTO people(name) VALUES (n)",
    "CREATE VIEW people_doubled AS SELECT id, name, dbl(id) AS d FROM people",
    "INSERT INTO people (name, mood, meta, photo, born, score) VALUES \
     ('Ann ''q''; \"dq\" \\\\ b', 'happy', '{\"k\": [1, 2]}', X'DEADBEEF', '2026-03-04 05:06:07', 12.50), \
     ('Bé Ünï 日本', 'sad', NULL, NULL, NULL, NULL), ('Line\\nbreak', 'ok', 'null', X'', NULL, -0.01)",
    "UPDATE people SET partner_id = 1 WHERE id = 2",
    "INSERT INTO pets (owner_id, name) VALUES (1, 'Rex'), (2, 'Tom')",
    "INSERT INTO b VALUES (1, NULL)",
    "INSERT INTO a VALUES (1, 1)",
    "UPDATE b SET a_id = 1",
];

/// `DROP DATABASE .. WITH (FORCE)` needs PostgreSQL 13: end the connections first.
async fn drop_pg(s: &dyn Session, db: &str) {
    exec(
        s,
        None,
        &format!("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db}'"),
    )
    .await;
    exec(s, None, &format!("DROP DATABASE IF EXISTS {db}")).await;
}

/// Allow stored functions on servers with binary logging (the test user has no SUPER).
async fn trust_function_creators(port: u16) {
    let mut cfg = config_at(DbKind::MySql, port);
    cfg.username = Some("root".into());
    let session = dbstudio_driver_mysql::MySqlDriver::new()
        .connect(&cfg, Some(&Secret::new("dbstudio_test")))
        .await
        .unwrap();
    // MariaDB without binary logging does not need it.
    if let Err(e) = execute_collect(
        &*session,
        QueryRequest::new("SET GLOBAL log_bin_trust_function_creators = 1"),
    )
    .await
    {
        eprintln!("log_bin_trust_function_creators on {port}: {e}");
    }
}

async fn scalar(s: &dyn Session, db: &str, sql: &str) -> Value {
    let mut req = QueryRequest::new(sql);
    req.database = Some(db.to_string());
    let r = execute_collect(s, req)
        .await
        .unwrap_or_else(|e| panic!("{db}: {sql}: {e}"));
    match &r[0].outcome {
        dbstudio_core::StatementOutcome::Rows(rs) => rs.rows[0][0].clone(),
        o => panic!("{sql}: {o:?}"),
    }
}

/// Everything `schema_model` knows about `db`, with the database name masked.
async fn model_of(s: &dyn Session, db: &str) -> String {
    let m = s
        .schema_model(dbstudio_core::SchemaModelRequest {
            scope: ObjectPath::root().child(ObjectKind::Database, db),
            tables: vec![],
            include_views: true,
            include_comments: true,
            detail: dbstudio_core::ModelDetail::Full,
            sample_size: None,
        })
        .await
        .unwrap();
    serde_json::to_string(&m).unwrap().replace(db, "<DB>")
}

struct Round {
    dir: tempfile::TempDir,
    rep: Arc<Rep>,
}

async fn backup_restore(
    cfg: &ConnectionConfig,
    s: &Arc<dyn Session>,
    src: &str,
    dst: &str,
    encrypted: bool,
    compression: Compression,
) -> Round {
    let dir = tempfile::tempdir().unwrap();
    let rep = Arc::new(Rep::default());
    let pass = Secret::new("correct horse");
    let mut sp = spec(src, encrypted, dir.path(), logical());
    sp.compression = compression;
    let out = run_backup(
        ctx(cfg, s, &rep, BTreeMap::new()),
        &sp,
        encrypted.then_some(&pass),
    )
    .await
    .unwrap();
    let file = PathBuf::from(&out.files[0]);
    let p = encrypted.then_some(&pass);
    let ins = inspect(&file, p).await.unwrap();
    assert_eq!(ins.encrypted, encrypted);
    assert_eq!(ins.preamble.unwrap()["database"], src);
    verify(&file, p, &*rep).await.unwrap();
    let rspec = RestoreSpec {
        file: file.to_string_lossy().into_owned(),
        target_connection_id: Uuid::nil(),
        target_database: Some(dst.to_string()),
        create_database: true,
        clean: false,
        objects: vec![],
        continue_on_error: false,
    };
    let res = run_restore(ctx(cfg, s, &rep, BTreeMap::new()), &rspec, p)
        .await
        .unwrap_or_else(|e| panic!("restore: {e}"));
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    Round { dir, rep }
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_full_round_trip_pg16_and_pg12() {
    if !enabled() {
        return;
    }
    for (port, encrypted, comp) in [
        (54316u16, true, Compression::Zstd { level: 3 }),
        (54312, false, Compression::Gzip { level: 6 }),
    ] {
        let (cfg, s) = connect_at(DbKind::Postgres, port).await;
        let n = unique();
        let (src, dst) = (format!("bk_it_full_src_{n}"), format!("bk_it_full_dst_{n}"));
        let _cleanup = DbGuard::new(DbKind::Postgres, port, [&src, &dst]);
        exec(&*s, None, &format!("CREATE DATABASE `{src}`")).await;
        for sql in PG_FULL {
            exec(&*s, Some(&src), sql).await;
        }
        let r = backup_restore(&cfg, &s, &src, &dst, encrypted, comp).await;
        assert!(r.rep.phases.lock().unwrap().iter().any(|p| p == "restore"));
        drop(r.dir);

        // Same structure on both sides.
        assert_eq!(
            model_of(&*s, &src).await,
            model_of(&*s, &dst).await,
            "port {port}"
        );
        // Same rows in every table (partitioned parent and partitions included).
        for t in [
            "people",
            "pets",
            "a",
            "b",
            "audit",
            "events",
            "events_2025",
            "events_2026",
            "checked",
        ] {
            let a = rows_of(&*s, pg_table(&src, "app", t)).await;
            let b = rows_of(&*s, pg_table(&dst, "app", t)).await;
            assert_eq!(a, b, "table {t} on {port}");
        }
        assert_eq!(rows_of(&*s, pg_table(&dst, "app", "people")).await.len(), 3);
        // Objects work on the restored database and give what the source gives.
        for sql in [
            "SELECT app.double(21)",
            "SELECT count(*) FROM app.people_doubled",
            "SELECT count(*) FROM app.mood_stats",
            "SELECT sum(n) FROM app.mood_stats",
            "SELECT count(*) FROM pg_inherits",
            "SELECT convalidated FROM pg_constraint WHERE conname = 'qty_pos'",
            "SELECT count(*) FROM pg_trigger WHERE tgname = 'people_audit'",
            "SELECT count(*) FROM pg_extension WHERE extname = 'citext'",
            "SELECT nextval('app.ticket_seq')",
            "SELECT nextval('app.people_id_seq')",
            "SELECT nextval('app.events_id_seq')",
        ] {
            assert_eq!(
                scalar(&*s, &src, sql).await,
                scalar(&*s, &dst, sql).await,
                "{sql} on {port}"
            );
        }
        assert_eq!(
            scalar(
                &*s,
                &dst,
                "SELECT convalidated FROM pg_constraint WHERE conname = 'qty_pos'"
            )
            .await,
            Value::Bool(false)
        );
        // Trigger and procedure are live in the copy.
        let before = scalar(&*s, &dst, "SELECT count(*) FROM app.audit").await;
        exec(&*s, Some(&dst), "CALL app.add_person('Zed')").await;
        let after = scalar(&*s, &dst, "SELECT count(*) FROM app.audit").await;
        match (before, after) {
            (Value::Int(b), Value::Int(a)) => assert_eq!(a, b + 1),
            o => panic!("{o:?}"),
        }
        for db in [&src, &dst] {
            drop_pg(&*s, db).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_and_mariadb_full_round_trip() {
    if !enabled() {
        return;
    }
    for (port, encrypted, comp) in [
        (33068u16, true, Compression::Zstd { level: 3 }),
        (33106, false, Compression::Gzip { level: 6 }),
    ] {
        trust_function_creators(port).await;
        let (cfg, s) = connect_at(DbKind::MySql, port).await;
        let n = unique();
        let (src, dst) = (
            format!("dbstudio_it_full_src_{n}"),
            format!("dbstudio_it_full_dst_{n}"),
        );
        let _cleanup = DbGuard::new(DbKind::MySql, port, [&src, &dst]);
        exec(&*s, None, &format!("CREATE DATABASE `{src}`")).await;
        for sql in MY_FULL {
            exec(&*s, Some(&src), sql).await;
        }
        let r = backup_restore(&cfg, &s, &src, &dst, encrypted, comp).await;
        drop(r.dir);
        assert_eq!(
            model_of(&*s, &src).await,
            model_of(&*s, &dst).await,
            "port {port}"
        );
        for t in ["people", "pets", "a", "b", "audit"] {
            let a = rows_of(&*s, my_table(&src, t)).await;
            let b = rows_of(&*s, my_table(&dst, t)).await;
            assert_eq!(a, b, "table {t} on {port}");
        }
        for sql in [
            "SELECT dbl(21)",
            "SELECT COUNT(*) FROM people_doubled",
            "SELECT COUNT(*) FROM information_schema.triggers WHERE trigger_schema = DATABASE()",
            "SELECT COUNT(*) FROM information_schema.routines WHERE routine_schema = DATABASE()",
            "SELECT COUNT(*) FROM information_schema.referential_constraints WHERE constraint_schema = DATABASE()",
            "SELECT AUTO_INCREMENT FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = 'people'",
        ] {
            assert_eq!(
                scalar(&*s, &src, sql).await,
                scalar(&*s, &dst, sql).await,
                "{sql} on {port}"
            );
        }
        let before = scalar(&*s, &dst, "SELECT COUNT(*) FROM audit").await;
        exec(&*s, Some(&dst), "CALL add_person('Zed')").await;
        let after = scalar(&*s, &dst, "SELECT COUNT(*) FROM audit").await;
        match (before, after) {
            (Value::Int(b), Value::Int(a)) => assert_eq!(a, b + 1),
            o => panic!("{o:?}"),
        }
        for db in [&src, &dst] {
            exec(&*s, None, &format!("DROP DATABASE IF EXISTS `{db}`")).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_guards_clean_and_failure_semantics() {
    if !enabled() {
        return;
    }
    let (cfg, s) = connect(DbKind::Postgres).await;
    let n = unique();
    let (src, dst) = (format!("bk_it_g_src_{n}"), format!("bk_it_g_dst_{n}"));
    let _cleanup = DbGuard::new(DbKind::Postgres, 54316, [&src, &dst]);
    exec(&*s, None, &format!("CREATE DATABASE `{src}`")).await;
    for sql in &PG_FULL[..12] {
        exec(&*s, Some(&src), sql).await;
    }
    exec(
        &*s,
        Some(&src),
        "INSERT INTO app.people (name) VALUES ('x')",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let rep = Arc::new(Rep::default());
    let out = run_backup(
        ctx(&cfg, &s, &rep, BTreeMap::new()),
        &spec(&src, false, dir.path(), logical()),
        None,
    )
    .await
    .unwrap();
    if let Ok(keep) = std::env::var("DBSTUDIO_KEEP") {
        std::fs::copy(&out.files[0], keep).unwrap();
    }
    let mut rspec = RestoreSpec {
        file: out.files[0].clone(),
        target_connection_id: Uuid::nil(),
        target_database: Some(dst.clone()),
        create_database: true,
        clean: false,
        objects: vec![],
        continue_on_error: false,
    };
    // Selected objects are not supported for integrated backups.
    let mut subset = rspec.clone();
    subset.objects = vec!["app.people".into()];
    assert_eq!(
        run_restore(ctx(&cfg, &s, &rep, BTreeMap::new()), &subset, None)
            .await
            .unwrap_err()
            .kind(),
        "unsupported"
    );
    run_restore(ctx(&cfg, &s, &rep, BTreeMap::new()), &rspec, None)
        .await
        .unwrap();
    // Into the populated database the single transaction fails and changes nothing.
    rspec.create_database = false;
    let before = rows_of(&*s, pg_table(&dst, "app", "people")).await;
    assert!(
        run_restore(ctx(&cfg, &s, &rep, BTreeMap::new()), &rspec, None)
            .await
            .is_err()
    );
    assert_eq!(rows_of(&*s, pg_table(&dst, "app", "people")).await, before);
    // With `clean` the content is replaced.
    exec(
        &*s,
        Some(&dst),
        "INSERT INTO app.people (name) VALUES ('extra')",
    )
    .await;
    rspec.clean = true;
    run_restore(ctx(&cfg, &s, &rep, BTreeMap::new()), &rspec, None)
        .await
        .unwrap();
    assert_eq!(
        rows_of(&*s, pg_table(&src, "app", "people")).await,
        rows_of(&*s, pg_table(&dst, "app", "people")).await
    );
    for db in [&src, &dst] {
        drop_pg(&*s, db).await;
    }
}

/// A profile with an SSH tunnel: the run opens its own tunnel (host key must be trusted first),
/// backs up and restores through it.
#[tokio::test(flavor = "multi_thread")]
async fn backup_and_restore_through_an_ssh_tunnel() {
    use dbstudio_backup::runner::{
        RunEnv, connect as runner_connect, execute_backup, execute_restore,
    };
    use dbstudio_core::{DriverRegistry, SshAuth, SshConfig};
    use dbstudio_store::{
        ConnectionProfile, ConnectionService, MemorySecretStore, SecretChange, SecretStore, Store,
        ssh_secret_account,
    };

    if !enabled() {
        return;
    }
    let (_, direct) = connect(DbKind::Postgres).await;
    let n = unique();
    let (src, dst) = (format!("bk_it_ssh_src_{n}"), format!("bk_it_ssh_dst_{n}"));
    let _cleanup = DbGuard::new(DbKind::Postgres, 54316, [&src, &dst]);
    exec(&*direct, None, &format!("CREATE DATABASE `{src}`")).await;
    for sql in &PG_FIXTURE[..6] {
        exec(&*direct, Some(&src), sql).await;
    }
    exec(
        &*direct,
        Some(&src),
        "INSERT INTO app.customers (name, balance) VALUES ('x', 1.5), ('y', 2.5)",
    )
    .await;

    let secrets = Arc::new(MemorySecretStore::new());
    let service = Arc::new(ConnectionService::new(
        Store::open_in_memory().unwrap(),
        secrets.clone(),
    ));
    let mut registry = DriverRegistry::new();
    registry.register(Arc::new(dbstudio_driver_postgres::PostgresDriver::new()));
    let mut config = ConnectionConfig::new(DbKind::Postgres, "via-ssh", "pg16");
    config.port = 5432;
    config.username = Some("dbstudio".into());
    config.database = Some("dbstudio".into());
    config.tls.mode = TlsMode::Disable;
    config.connect_timeout_ms = Some(8000);
    config.ssh = Some(SshConfig {
        host: "127.0.0.1".into(),
        port: 22022,
        username: "dbstudio".into(),
        auth: SshAuth::Password,
        keepalive_secs: None,
        connect_timeout_ms: Some(8000),
    });
    let conn_id = config.id;
    service
        .save(
            ConnectionProfile {
                config,
                group_id: None,
                color: None,
                save_password: true,
                has_password: false,
                sort_order: 0,
                created_at: String::new(),
                updated_at: String::new(),
            },
            SecretChange::Set {
                value: Secret::new("dbstudio_test"),
            },
        )
        .unwrap();
    secrets
        .set_account(&ssh_secret_account(conn_id), &Secret::new("dbstudio_ssh"))
        .unwrap();
    let env = RunEnv {
        service: service.clone(),
        secrets: secrets.clone(),
        registry: Arc::new(registry),
        locks_dir: std::env::temp_dir().join("dbstudio-it-locks"),
        work_dir: std::env::temp_dir().join("dbstudio-it-work"),
    };
    // An unknown host key stops the run (nobody can confirm it).
    let fingerprint = match runner_connect(&env, conn_id).await.unwrap_err() {
        dbstudio_core::Error::SshHostKey {
            host,
            port,
            key_type,
            fingerprint,
            ..
        } => {
            assert_eq!((host.as_str(), port), ("127.0.0.1", 22022));
            service
                .store()
                .known_host_add(&host, port, &key_type, &fingerprint)
                .unwrap();
            fingerprint
        }
        e => panic!("{e:?}"),
    };
    assert!(fingerprint.starts_with("SHA256:"));
    let conn = runner_connect(&env, conn_id).await.unwrap();
    assert!(conn.tunnel_addr.is_some());
    drop(conn);

    let dir = tempfile::tempdir().unwrap();
    let rep = Arc::new(Rep::default());
    let mut sp = spec(&src, false, dir.path(), logical());
    sp.connection_id = conn_id;
    let out = execute_backup(&env, &sp, None, Some("ssh".into()), rep.clone())
        .await
        .unwrap();
    let res = execute_restore(
        &env,
        &RestoreSpec {
            file: out.files[0].clone(),
            target_connection_id: conn_id,
            target_database: Some(dst.clone()),
            create_database: true,
            clean: false,
            objects: vec![],
            continue_on_error: false,
        },
        None,
        rep,
    )
    .await
    .unwrap();
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    assert_eq!(
        rows_of(&*direct, pg_table(&src, "app", "customers")).await,
        rows_of(&*direct, pg_table(&dst, "app", "customers")).await
    );
    for db in [&src, &dst] {
        drop_pg(&*direct, db).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Review fixes
// ---------------------------------------------------------------------------------------------

/// `GENERATED ALWAYS AS IDENTITY` columns need `OVERRIDING SYSTEM VALUE` in the script, and the
/// identity sequence must continue after the restored rows.
#[tokio::test(flavor = "multi_thread")]
async fn pg_identity_always_columns_restore_and_continue() {
    if !enabled() {
        return;
    }
    for port in [54316u16, 54312] {
        let (cfg, s) = connect_at(DbKind::Postgres, port).await;
        let n = unique();
        let (src, dst) = (format!("bk_it_id_src_{n}"), format!("bk_it_id_dst_{n}"));
        let _cleanup = DbGuard::new(DbKind::Postgres, port, [&src, &dst]);
        exec(&*s, None, &format!("CREATE DATABASE `{src}`")).await;
        for sql in [
            "CREATE TABLE ids (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name text)",
            "CREATE TABLE ids_def (id int GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, name text)",
            "INSERT INTO ids (name) VALUES ('a'), ('b'), ('c')",
            "INSERT INTO ids_def (name) VALUES ('d'), ('e')",
            "DELETE FROM ids WHERE id = 2",
        ] {
            exec(&*s, Some(&src), sql).await;
        }
        let r = backup_restore(&cfg, &s, &src, &dst, false, Compression::None).await;
        drop(r.dir);
        let table = |db: &str, t: &str| {
            ObjectPath::root()
                .child(ObjectKind::Database, db)
                .child(ObjectKind::Schema, "public")
                .child(ObjectKind::Table, t)
        };
        for t in ["ids", "ids_def"] {
            assert_eq!(
                rows_of(&*s, table(&src, t)).await,
                rows_of(&*s, table(&dst, t)).await,
                "{t} on {port}"
            );
        }
        // The next value is the same on both sides (the gap left by the deleted row stays).
        exec(&*s, Some(&src), "INSERT INTO ids (name) VALUES ('z')").await;
        exec(&*s, Some(&dst), "INSERT INTO ids (name) VALUES ('z')").await;
        assert_eq!(
            scalar(&*s, &src, "SELECT max(id) FROM ids").await,
            scalar(&*s, &dst, "SELECT max(id) FROM ids").await
        );
        assert_eq!(
            scalar(&*s, &dst, "SELECT max(id) FROM ids").await,
            Value::Int(4)
        );
        for db in [&src, &dst] {
            drop_pg(&*s, db).await;
        }
    }
}

/// More tables than one schema model lists (2000): the backup lists schema by schema and a
/// schema that is too big alone fails loudly instead of producing an incomplete backup.
#[tokio::test(flavor = "multi_thread")]
async fn big_databases_are_not_silently_truncated() {
    if !enabled() {
        return;
    }
    let (cfg, s) = connect(DbKind::Postgres).await;
    let n = unique();
    let (src, dst, big) = (
        format!("bk_it_big_src_{n}"),
        format!("bk_it_big_dst_{n}"),
        format!("bk_it_big2_{n}"),
    );
    let _cleanup = DbGuard::new(DbKind::Postgres, 54316, [&src, &dst, &big]);
    exec(&*s, None, &format!("CREATE DATABASE `{src}`")).await;
    exec(&*s, Some(&src), "CREATE SCHEMA s1").await;
    exec(&*s, Some(&src), "CREATE SCHEMA s2").await;
    for schema in ["s1", "s2"] {
        exec(
            &*s,
            Some(&src),
            &format!(
                "DO $$ BEGIN FOR i IN 1..1100 LOOP EXECUTE format('CREATE TABLE {schema}.t%s (id int)', i); END LOOP; END $$"
            ),
        )
        .await;
    }
    exec(&*s, Some(&src), "INSERT INTO s1.t1 VALUES (7)").await;
    let r = backup_restore(&cfg, &s, &src, &dst, false, Compression::Gzip { level: 1 }).await;
    drop(r.dir);
    let count = "SELECT count(*) FROM information_schema.tables WHERE table_schema IN ('s1', 's2')";
    assert_eq!(scalar(&*s, &dst, count).await, Value::Int(2200));
    assert_eq!(
        scalar(&*s, &dst, "SELECT id FROM s1.t1").await,
        Value::Int(7)
    );

    // One schema with more than 2000 tables cannot be listed: an error, no file.
    exec(&*s, None, &format!("CREATE DATABASE {big}")).await;
    exec(
        &*s,
        Some(&big),
        "DO $$ BEGIN FOR i IN 1..2100 LOOP EXECUTE format('CREATE TABLE t%s (id int)', i); END LOOP; END $$",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let rep = Arc::new(Rep::default());
    let err = run_backup(
        ctx(&cfg, &s, &rep, BTreeMap::new()),
        &spec(&big, false, dir.path(), logical()),
        None,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("more tables"), "{err}");
    let files: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
    assert!(files.is_empty(), "{files:?}");
    for db in [&src, &dst, &big] {
        drop_pg(&*s, db).await;
    }
}

/// Partitions are loaded through their parent when it is exported; exported alone, a partition
/// keeps its own rows. Checked with data-only exports (no DDL to tell them apart).
#[tokio::test(flavor = "multi_thread")]
async fn partitions_are_exported_once_from_the_catalog() {
    use dbstudio_export::{
        ExportContext, ExportDestination, ExportFormat, ExportSource, ExportSpec, SqlOptions,
        ValueFormat, run_export,
    };
    if !enabled() {
        return;
    }
    let (_, s) = connect(DbKind::Postgres).await;
    let n = unique();
    let src = format!("bk_it_part_{n}");
    let _cleanup = DbGuard::new(DbKind::Postgres, 54316, [&src]);
    exec(&*s, None, &format!("CREATE DATABASE `{src}`")).await;
    for sql in [
        "CREATE TABLE ev (id int, at date) PARTITION BY RANGE (at)",
        "CREATE TABLE ev_a PARTITION OF ev FOR VALUES FROM ('2025-01-01') TO ('2026-01-01')",
        "CREATE TABLE ev_b PARTITION OF ev FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')",
        "INSERT INTO ev VALUES (1, '2025-05-05'), (2, '2026-05-05'), (3, '2026-06-06')",
    ] {
        exec(&*s, Some(&src), sql).await;
    }
    let dir = tempfile::tempdir().unwrap();
    let export = |source: ExportSource, name: &str| {
        let s = s.clone();
        let path = dir.path().join(name);
        async move {
            let rep: Arc<dyn JobReporter> = Arc::new(Rep::default());
            run_export(
                ExportContext {
                    session: s.clone(),
                    kind: DbKind::Postgres,
                    dialect: s.script_dialect(),
                    reporter: rep,
                },
                ExportSpec {
                    connection_id: Uuid::nil(),
                    source,
                    content: ExportContent::Data,
                    format: ExportFormat::Sql(SqlOptions::default()),
                    destination: ExportDestination {
                        path: path.to_string_lossy().into_owned(),
                        overwrite: true,
                    },
                    values: ValueFormat::default(),
                },
            )
            .await
            .unwrap_or_else(|e| panic!("export {}: {e}", path.display()));
            std::fs::read_to_string(path).unwrap()
        }
    };
    let db = ObjectPath::root().child(ObjectKind::Database, &src);
    let whole = export(ExportSource::Database { path: db.clone() }, "whole.sql").await;
    assert_eq!(
        whole.matches("INSERT INTO \"public\".\"ev\"").count(),
        1,
        "{whole}"
    );
    assert!(
        !whole.contains("\"ev_a\"") && !whole.contains("\"ev_b\""),
        "{whole}"
    );
    // Only a partition chosen: its own rows, once.
    let one = export(
        ExportSource::Tables {
            paths: vec![
                db.child(ObjectKind::Folder, "schemas")
                    .child(ObjectKind::Schema, "public")
                    .child(ObjectKind::Folder, "tables")
                    .child(ObjectKind::Table, "ev_b"),
            ],
        },
        "one.sql",
    )
    .await;
    assert!(one.contains("INSERT INTO \"public\".\"ev_b\""), "{one}");
    assert!(
        one.contains("(2,") && one.contains("(3,") && !one.contains("(1,"),
        "{one}"
    );
    drop_pg(&*s, &src).await;
}

/// `clean` on MySQL replaces the database but keeps its character set and collation.
#[tokio::test(flavor = "multi_thread")]
async fn mysql_clean_keeps_the_database_charset() {
    if !enabled() {
        return;
    }
    for port in [33068u16, 33106] {
        let (cfg, s) = connect_at(DbKind::MySql, port).await;
        let n = unique();
        let (src, dst) = (
            format!("dbstudio_it_cl_src_{n}"),
            format!("dbstudio_it_cl_dst_{n}"),
        );
        let _cleanup = DbGuard::new(DbKind::MySql, port, [&src, &dst]);
        exec(&*s, None, &format!("CREATE DATABASE `{src}`")).await;
        exec(
            &*s,
            Some(&src),
            "CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(10))",
        )
        .await;
        exec(&*s, Some(&src), "INSERT INTO t VALUES (1, 'a')").await;
        let dir = tempfile::tempdir().unwrap();
        let rep = Arc::new(Rep::default());
        let out = run_backup(
            ctx(&cfg, &s, &rep, BTreeMap::new()),
            &spec(&src, false, dir.path(), logical()),
            None,
        )
        .await
        .unwrap();
        exec(
            &*s,
            None,
            &format!("CREATE DATABASE `{dst}` CHARACTER SET latin1 COLLATE latin1_general_ci"),
        )
        .await;
        exec(&*s, Some(&dst), "CREATE TABLE extra (id INT)").await;
        let rspec = RestoreSpec {
            file: out.files[0].clone(),
            target_connection_id: Uuid::nil(),
            target_database: Some(dst.clone()),
            create_database: false,
            clean: true,
            objects: vec![],
            continue_on_error: false,
        };
        run_restore(ctx(&cfg, &s, &rep, BTreeMap::new()), &rspec, None)
            .await
            .unwrap();
        let q = "SELECT CONCAT(DEFAULT_CHARACTER_SET_NAME, '/', DEFAULT_COLLATION_NAME) \
                 FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = DATABASE()";
        assert_eq!(
            scalar(&*s, &dst, q).await,
            Value::Text("latin1/latin1_general_ci".into()),
            "port {port}"
        );
        assert_eq!(
            scalar(&*s, &dst, "SELECT COUNT(*) FROM t").await,
            Value::Int(1)
        );
        assert_eq!(
            scalar(
                &*s,
                &dst,
                "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = 'extra'"
            )
            .await,
            Value::Int(0)
        );
        // A clean restore must name its target explicitly.
        let mut unnamed = rspec.clone();
        unnamed.target_database = None;
        assert!(
            run_restore(ctx(&cfg, &s, &rep, BTreeMap::new()), &unnamed, None)
                .await
                .is_err()
        );
        for db in [&src, &dst] {
            exec(&*s, None, &format!("DROP DATABASE IF EXISTS `{db}`")).await;
        }
    }
}

/// Two backups of the same job in the same second must end up as two files.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_backups_never_share_or_overwrite_a_file() {
    if !enabled() {
        return;
    }
    let (cfg, s) = connect(DbKind::Postgres).await;
    let n = unique();
    let src = format!("bk_it_same_{n}");
    let _cleanup = DbGuard::new(DbKind::Postgres, 54316, [&src]);
    exec(&*s, None, &format!("CREATE DATABASE `{src}`")).await;
    exec(&*s, Some(&src), "CREATE TABLE t (id int)").await;
    let dir = tempfile::tempdir().unwrap();
    let mut sp = spec(&src, false, dir.path(), logical());
    sp.destination = Destination::LocalDir {
        path: dir.path().to_string_lossy().into_owned(),
        file_template: "same-name.dbsbak".into(),
    };
    let run = |sp: BackupSpec| {
        let (cfg, s) = (cfg.clone(), s.clone());
        async move {
            let rep = Arc::new(Rep::default());
            run_backup(ctx(&cfg, &s, &rep, BTreeMap::new()), &sp, None).await
        }
    };
    let (a, b) = tokio::join!(run(sp.clone()), run(sp.clone()));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(a.files[0], b.files[0]);
    for f in [&a.files[0], &b.files[0]] {
        verify(Path::new(f), None, &Rep::default()).await.unwrap();
    }
    let all: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(all.iter().all(|n| !n.ends_with(".part")), "{all:?}");
    drop_pg(&*s, &src).await;
}

/// Partial restore: five tables with foreign keys, restore two. The restored tables exist and
/// hold their rows; the constraint that points at a table that is not restored is dropped and
/// the log says so.
async fn partial_restore_case(kind: DbKind, port: u16) {
    let (cfg, s) = connect_at(kind, port).await;
    let (src, dst) = (
        format!("dbstudio_it_part_src_{}", unique()),
        format!("dbstudio_it_part_dst_{}", unique()),
    );
    let _cleanup = DbGuard::new(kind, port, [&src, &dst]);
    let pg = kind == DbKind::Postgres;
    exec(&*s, None, &format!("CREATE DATABASE {src}")).await;
    let ddl = [
        "CREATE TABLE t1 (id INT PRIMARY KEY, v VARCHAR(20))",
        "CREATE TABLE t2 (id INT PRIMARY KEY, t1_id INT, CONSTRAINT fk_t2_t1 FOREIGN KEY (t1_id) REFERENCES t1 (id))",
        "CREATE TABLE t3 (id INT PRIMARY KEY, t2_id INT, CONSTRAINT fk_t3_t2 FOREIGN KEY (t2_id) REFERENCES t2 (id))",
        "CREATE TABLE t4 (id INT PRIMARY KEY, v VARCHAR(20))",
        "CREATE TABLE t5 (id INT PRIMARY KEY, t4_id INT, CONSTRAINT fk_t5_t4 FOREIGN KEY (t4_id) REFERENCES t4 (id))",
        "INSERT INTO t1 VALUES (1, 'a'), (2, 'b')",
        "INSERT INTO t2 VALUES (1, 1), (2, 2), (3, 2)",
        "INSERT INTO t3 VALUES (1, 1)",
        "INSERT INTO t4 VALUES (1, 'x'), (2, 'y')",
        "INSERT INTO t5 VALUES (1, 1)",
    ];
    for sql in ddl {
        exec(&*s, Some(&src), sql).await;
    }
    let dir = tempfile::tempdir().unwrap();
    let rep = Arc::new(Rep::default());
    let out = run_backup(
        ctx(&cfg, &s, &rep, BTreeMap::new()),
        &spec(&src, false, dir.path(), logical()),
        None,
    )
    .await
    .unwrap();
    let file = PathBuf::from(&out.files[0]);
    let ins = inspect(&file, None).await.unwrap();
    assert_eq!(ins.preamble.as_ref().unwrap()["formatVersion"], 3);
    let tables: Vec<_> = ins
        .objects
        .iter()
        .filter(|o| o.kind == ObjectKind::Table)
        .map(|o| o.name.clone())
        .collect();
    assert_eq!(tables.len(), 5, "{tables:?}");
    let rspec = RestoreSpec {
        file: file.to_string_lossy().into_owned(),
        target_connection_id: Uuid::nil(),
        target_database: Some(dst.clone()),
        create_database: true,
        clean: false,
        objects: vec!["t2".into(), "t4".into()],
        continue_on_error: false,
    };
    let res = run_restore(ctx(&cfg, &s, &rep, BTreeMap::new()), &rspec, None)
        .await
        .unwrap_or_else(|e| panic!("partial restore: {e}"));
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    assert_eq!(res.objects, 2);
    let listing = if pg {
        "SELECT string_agg(table_name, ',' ORDER BY table_name) FROM information_schema.tables WHERE table_schema = 'public'"
    } else {
        "SELECT GROUP_CONCAT(table_name ORDER BY table_name) FROM information_schema.tables WHERE table_schema = DATABASE()"
    };
    let v = scalar(&*s, &dst, listing).await;
    assert!(matches!(&v, Value::Text(t) if t == "t2,t4"), "{v:?}");
    assert!(matches!(
        scalar(&*s, &dst, "SELECT COUNT(*) FROM t2").await,
        Value::Int(3) | Value::UInt(3)
    ));
    assert!(matches!(
        scalar(&*s, &dst, "SELECT COUNT(*) FROM t4").await,
        Value::Int(2) | Value::UInt(2)
    ));
    let fks = if pg {
        "SELECT COUNT(*) FROM information_schema.table_constraints WHERE constraint_type = 'FOREIGN KEY' AND table_schema = 'public'"
    } else {
        "SELECT COUNT(*) FROM information_schema.table_constraints WHERE constraint_type = 'FOREIGN KEY' AND table_schema = DATABASE()"
    };
    // t2 -> t1 is dropped (t1 is not restored); t5 -> t4 is not restored with t5.
    assert!(matches!(
        scalar(&*s, &dst, fks).await,
        Value::Int(0) | Value::UInt(0)
    ));
    let logs = rep.logs.lock().unwrap().join("\n");
    assert!(
        logs.contains("references") && logs.contains("t1"),
        "the skipped constraint is reported: {logs}"
    );
    // A table that is not in the backup is refused.
    let mut bad = rspec.clone();
    bad.objects = vec!["nope".into()];
    assert!(
        run_restore(ctx(&cfg, &s, &rep, BTreeMap::new()), &bad, None)
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn partial_restore_postgres_and_mysql() {
    if !enabled() {
        return;
    }
    partial_restore_case(DbKind::Postgres, 54316).await;
    partial_restore_case(DbKind::MySql, 33068).await;
}
