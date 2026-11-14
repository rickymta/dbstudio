//! Database scenarios shared by the PostgreSQL and MySQL integration tests (`DBSTUDIO_IT=1`).
#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use dbstudio_core::DdlOptions;
use dbstudio_core::{
    ConnectionConfig, DbKind, Driver, ObjectKind, ObjectPath, QueryRequest, Secret, Session,
    StatementOutcome, TlsConfig, TlsMode, Value, execute_collect,
};
use dbstudio_export::{
    CsvOptions, ExportContent, ExportContext, ExportDestination, ExportFormat, ExportSource,
    ExportSpec, ExportTuning, ImportFormat, ImportMode, ImportSpec, ImportTarget, JsonLayout,
    JsonOptions, SqlOptions, TextEncoding, ValueFormat, XlsxOptions, run_export, run_export_tuned,
    run_import,
};
use uuid::Uuid;

use crate::common::Recorder;

pub fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").is_ok_and(|v| v == "1")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Pg,
    My,
}

/// A server with the fixture schema loaded.
pub struct Env {
    pub flavor: Flavor,
    pub label: String,
    pub port: u16,
    /// Database (schema namespace) holding the fixture; unique per environment.
    pub src_db: String,
    /// Session on the database that holds the fixture.
    pub src: Arc<dyn Session>,
    /// Session that may create databases.
    pub admin: Arc<dyn Session>,
    /// Databases this environment created; dropped with it, also when the test fails.
    created: std::sync::Mutex<Vec<String>>,
}

impl Drop for Env {
    fn drop(&mut self) {
        let dbs = std::mem::take(&mut *self.created.lock().unwrap_or_else(|e| e.into_inner()));
        let (flavor, port) = (self.flavor, self.port);
        // A fresh thread and runtime: this may run while the test runtime is shutting down.
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                let admin = match flavor {
                    Flavor::Pg => pg_connect(port, "dbstudio").await,
                    Flavor::My => my_connect(port).await,
                };
                for db in dbs {
                    let sql = match flavor {
                        Flavor::Pg => format!(
                            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                             WHERE datname = '{db}' AND pid <> pg_backend_pid(); \
                             DROP DATABASE IF EXISTS {db}"
                        ),
                        Flavor::My => format!("DROP DATABASE IF EXISTS {db}"),
                    };
                    for stmt in sql.split(';') {
                        let _ = execute_collect(&*admin, QueryRequest::new(stmt)).await;
                    }
                }
                let _ = admin.close().await;
            });
        })
        .join();
    }
}

pub fn secret() -> Secret {
    Secret::new("dbstudio_test")
}

async fn pg_connect(port: u16, database: &str) -> Arc<dyn Session> {
    let mut cfg = ConnectionConfig::new(DbKind::Postgres, "it", "127.0.0.1");
    cfg.port = port;
    cfg.database = Some(database.into());
    cfg.username = Some("dbstudio".into());
    cfg.tls = TlsConfig {
        mode: TlsMode::Prefer,
        ..TlsConfig::default()
    };
    cfg.connect_timeout_ms = Some(5000);
    let s = dbstudio_driver_postgres::PostgresDriver::new()
        .connect(&cfg, Some(&secret()))
        .await
        .unwrap_or_else(|e| panic!("connect {database}: {e}"));
    Arc::from(s)
}

async fn my_connect(port: u16) -> Arc<dyn Session> {
    let mut cfg = ConnectionConfig::new(DbKind::MySql, "it", "127.0.0.1");
    cfg.port = port;
    cfg.username = Some("root".into());
    cfg.tls = TlsConfig {
        mode: TlsMode::Disable,
        ..TlsConfig::default()
    };
    cfg.connect_timeout_ms = Some(5000);
    let s = dbstudio_driver_mysql::MySqlDriver::new()
        .connect(&cfg, Some(&secret()))
        .await
        .unwrap_or_else(|e| panic!("connect mysql {port}: {e}"));
    Arc::from(s)
}

pub async fn run(s: &dyn Session, database: Option<&str>, sql: &str) {
    let mut req = QueryRequest::new(sql);
    req.database = database.map(str::to_string);
    if let Err(e) = execute_collect(s, req).await {
        panic!("sql failed: {e}\n{}", &sql[..sql.len().min(400)]);
    }
}

pub async fn text_of(s: &dyn Session, database: Option<&str>, sql: &str) -> String {
    let mut req = QueryRequest::new(sql);
    req.database = database.map(str::to_string);
    let results = execute_collect(s, req)
        .await
        .unwrap_or_else(|e| panic!("sql failed: {e}\n{sql}"));
    match &results[0].outcome {
        StatementOutcome::Rows(rs) => match &rs.rows[0][0] {
            Value::Text(t) | Value::Decimal(t) => t.clone(),
            Value::Int(i) => i.to_string(),
            Value::UInt(i) => i.to_string(),
            Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            other => panic!("unexpected {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

const PG_FIXTURE: &str = r#"
DROP TABLE IF EXISTS exp_b;
DROP TABLE IF EXISTS exp_a;
CREATE TABLE exp_a (
  id bigint PRIMARY KEY, name text, qty integer NOT NULL DEFAULT 0, price numeric(12,2),
  ratio double precision, flag boolean, born date, created timestamp, seen timestamptz,
  blob bytea, doc jsonb, uid uuid, tags text[]
);
COMMENT ON TABLE exp_a IS 'main table';
COMMENT ON COLUMN exp_a.name IS 'display name';
CREATE INDEX exp_a_name_idx ON exp_a (name);
INSERT INTO exp_a
SELECT g,
  CASE WHEN g % 7 = 0 THEN NULL ELSE 'row ' || g || E' "q" \\ ''s\nline ü漢' END,
  g % 100, (g * 1.25)::numeric(12,2), g / 7.0, g % 2 = 0, date '2020-01-01' + (g % 365),
  timestamp '2020-01-01 00:00:00' + g * interval '1 second',
  timestamptz '2020-01-01 00:00:00+00' + g * interval '1 minute',
  decode(md5(g::text), 'hex'), jsonb_build_object('n', g, 's', 'x'), md5(g::text)::uuid,
  ARRAY['a' || g, 'b c', NULL]
FROM generate_series(1, 2500) g;
CREATE TABLE exp_b (
  id integer PRIMARY KEY, a_id bigint NOT NULL REFERENCES exp_a (id), note text
);
INSERT INTO exp_b SELECT g, g * 10, 'note ' || g FROM generate_series(1, 50) g;
"#;

const MY_FIXTURE: &str = r#"
DROP TABLE IF EXISTS exp_b;
DROP TABLE IF EXISTS exp_a;
CREATE TABLE exp_a (
  id BIGINT PRIMARY KEY, name TEXT, qty INT NOT NULL DEFAULT 0, price DECIMAL(12,2),
  ratio DOUBLE, flag TINYINT(1), born DATE, created DATETIME(3), bin BLOB, doc JSON,
  KEY exp_a_name_idx (name(50))
) COMMENT='main table';
INSERT INTO exp_a
SELECT g,
  CASE WHEN g % 7 = 0 THEN NULL ELSE CONCAT('row ', g, ' "q" \\ ''s', CHAR(10), 'line ü漢') END,
  g % 100, g * 1.25, g / 7.0, g % 2 = 0, DATE_ADD('2020-01-01', INTERVAL g % 365 DAY),
  TIMESTAMPADD(SECOND, g, '2020-01-01 00:00:00.000'), UNHEX(MD5(g)),
  CONCAT('{"n": ', g, ', "s": "x"}')
FROM (
  SELECT a.n + b.n * 10 + c.n * 100 + d.n * 1000 + 1 AS g
  FROM (SELECT 0 n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) a,
       (SELECT 0 n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) b,
       (SELECT 0 n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) c,
       (SELECT 0 n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) d
) seq WHERE g <= 2500;
CREATE TABLE exp_b (
  id INT PRIMARY KEY, a_id BIGINT NOT NULL, note TEXT,
  CONSTRAINT exp_b_fk FOREIGN KEY (a_id) REFERENCES exp_a (id)
);
INSERT INTO exp_b SELECT g, g * 10, CONCAT('note ', g)
FROM (SELECT a.n + b.n * 10 + 1 AS g FROM
  (SELECT 0 n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) a,
  (SELECT 0 n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4) b) s WHERE g <= 50;
"#;

/// Statements of a multi-statement fixture, one request each.
async fn run_script(s: &dyn Session, database: Option<&str>, script: &str) {
    for stmt in dbstudio_sql::split(s.script_dialect(), script) {
        let text: String = script
            .chars()
            .skip(stmt.start as usize)
            .take((stmt.end - stmt.start) as usize)
            .collect();
        if !text.trim().is_empty() {
            run(s, database, &text).await;
        }
    }
}

impl Env {
    pub async fn postgres(port: u16, label: &str) -> Env {
        let src_db = format!("exp_it_src_{}", label.replace('-', "_"));
        let admin = pg_connect(port, "dbstudio").await;
        recreate_pg_db(&*admin, &src_db).await;
        let src = pg_connect(port, &src_db).await;
        run_script(&*src, None, PG_FIXTURE).await;
        Env {
            flavor: Flavor::Pg,
            label: label.into(),
            port,
            created: std::sync::Mutex::new(vec![src_db.clone()]),
            src_db,
            src,
            admin,
        }
    }

    pub async fn mysql(port: u16, label: &str) -> Env {
        let src_db = format!("exp_it_src_{}", label.replace('-', "_"));
        let admin = my_connect(port).await;
        run(&*admin, None, &format!("DROP DATABASE IF EXISTS {src_db}")).await;
        run(&*admin, None, &format!("CREATE DATABASE {src_db}")).await;
        run_script(&*admin, Some(&src_db), MY_FIXTURE).await;
        Env {
            flavor: Flavor::My,
            label: label.into(),
            port,
            created: std::sync::Mutex::new(vec![src_db.clone()]),
            src_db,
            src: admin.clone(),
            admin,
        }
    }

    pub fn kind(&self) -> DbKind {
        match self.flavor {
            Flavor::Pg => DbKind::Postgres,
            Flavor::My => DbKind::MySql,
        }
    }

    /// The schema / database node of the fixture.
    pub fn scope(&self) -> ObjectPath {
        let db = ObjectPath::root().child(ObjectKind::Database, &self.src_db);
        match self.flavor {
            Flavor::Pg => db
                .child(ObjectKind::Folder, "schemas")
                .child(ObjectKind::Schema, "public"),
            Flavor::My => db,
        }
    }

    pub fn table(&self, name: &str) -> ObjectPath {
        self.scope()
            .child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, name)
    }

    pub fn ctx(&self, session: &Arc<dyn Session>, reporter: Arc<Recorder>) -> ExportContext {
        ExportContext {
            session: Arc::clone(session),
            kind: self.kind(),
            dialect: session.script_dialect(),
            reporter,
        }
    }

    /// Database the fixture tables live in (for requests that take one).
    pub fn src_database(&self) -> Option<&str> {
        match self.flavor {
            Flavor::Pg => None,
            Flavor::My => Some(&self.src_db),
        }
    }

    /// `count:checksum` of a table on `session` (in `database` for MySQL).
    pub async fn fingerprint(
        &self,
        session: &dyn Session,
        database: Option<&str>,
        table: &str,
    ) -> String {
        let sql = match (self.flavor, table) {
            (Flavor::Pg, t) => format!(
                "SELECT count(*)::text || ':' || coalesce(md5(string_agg(t::text, '|' ORDER BY id)), '') FROM {t} t"
            ),
            (Flavor::My, "exp_a") => "SELECT CONCAT(COUNT(*), ':', COALESCE(SUM(CRC32(CONCAT_WS('|', id, IFNULL(name, '~null~'), qty, price, ratio, flag, born, created, HEX(bin), doc))), 0)) FROM exp_a".to_string(),
            (Flavor::My, t) => format!(
                "SELECT CONCAT(COUNT(*), ':', COALESCE(SUM(CRC32(CONCAT_WS('|', id, a_id, IFNULL(note, '~null~')))), 0)) FROM {t}"
            ),
        };
        text_of(session, database, &sql).await
    }

    /// Empty destination: a fresh database. Returns the session and the database name MySQL
    /// scripts must run in.
    pub async fn destination(&self, tag: &str) -> (Arc<dyn Session>, Option<String>) {
        let db = format!("exp_it_dst_{}_{tag}", self.label.replace('-', "_"));
        self.created.lock().unwrap().push(db.clone());
        match self.flavor {
            Flavor::Pg => {
                recreate_pg_db(&*self.admin, &db).await;
                (pg_connect(self.port, &db).await, None)
            }
            Flavor::My => {
                run(&*self.admin, None, &format!("DROP DATABASE IF EXISTS {db}")).await;
                run(&*self.admin, None, &format!("CREATE DATABASE {db}")).await;
                (self.admin.clone(), Some(db))
            }
        }
    }

    pub async fn table_count(&self, session: &dyn Session, database: Option<&str>) -> String {
        let sql = match self.flavor {
            Flavor::Pg => "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'public' AND table_type = 'BASE TABLE'".to_string(),
            Flavor::My => format!(
                "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '{}' AND table_type = 'BASE TABLE'",
                database.unwrap_or(&self.src_db)
            ),
        };
        text_of(session, database, &sql).await
    }

    pub async fn foreign_key_count(&self, session: &dyn Session, database: Option<&str>) -> String {
        let sql = match self.flavor {
            Flavor::Pg => "SELECT count(*) FROM information_schema.table_constraints WHERE constraint_schema = 'public' AND constraint_type = 'FOREIGN KEY'".to_string(),
            Flavor::My => format!(
                "SELECT COUNT(*) FROM information_schema.table_constraints WHERE constraint_schema = '{}' AND constraint_type = 'FOREIGN KEY'",
                database.unwrap_or(&self.src_db)
            ),
        };
        text_of(session, database, &sql).await
    }
}

async fn recreate_pg_db(admin: &dyn Session, db: &str) {
    run(
        admin,
        None,
        &format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db}' AND pid <> pg_backend_pid()"
        ),
    )
    .await;
    run(admin, None, &format!("DROP DATABASE IF EXISTS {db}")).await;
    run(admin, None, &format!("CREATE DATABASE {db}")).await;
}

pub fn export_spec(
    source: ExportSource,
    content: ExportContent,
    format: ExportFormat,
    dest: &Path,
) -> ExportSpec {
    ExportSpec {
        connection_id: Uuid::nil(),
        source,
        content,
        format,
        destination: ExportDestination {
            path: dest.to_string_lossy().into_owned(),
            overwrite: false,
        },
        values: ValueFormat::default(),
    }
}

pub fn sql_options_unqualified() -> SqlOptions {
    SqlOptions {
        ddl: DdlOptions {
            qualify_schema: false,
            ..DdlOptions::default()
        },
        insert_batch_rows: 250,
        ..SqlOptions::default()
    }
}

fn zip_entry_names(path: &str) -> Vec<String> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    (0..z.len())
        .map(|i| z.by_index(i).unwrap().name().unwrap().to_string())
        .collect()
}

fn zip_entry(path: &str, name: &str) -> Vec<u8> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut data = Vec::new();
    z.by_name(name).unwrap().read_to_end(&mut data).unwrap();
    data
}

// ---------------------------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------------------------

/// "Both" as SQL replays into an empty database: tables, rows and checksums match.
pub async fn sql_round_trip(env: &Env) {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("dump.sql");
    let report = run_export(
        env.ctx(&env.src, Recorder::new()),
        export_spec(
            ExportSource::Schema { path: env.scope() },
            ExportContent::Both,
            ExportFormat::Sql(sql_options_unqualified()),
            &dest,
        ),
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] export failed: {e}", env.label));
    assert_eq!(report.objects, 2);
    assert_eq!(report.rows, 2550);
    assert!(report.warnings.iter().all(|w| !w.contains("error")));

    let (dst, database) = env.destination("sql").await;
    let imported = run_import(
        env.ctx(&dst, Recorder::new()),
        ImportSpec {
            connection_id: Uuid::nil(),
            file: dest.to_string_lossy().into_owned(),
            format: ImportFormat::Sql {
                continue_on_error: false,
            },
            target: ImportTarget::Script {
                database: database.clone(),
            },
            mapping: vec![],
            on_conflict: None,
            mode: ImportMode::Batches { rows: 1000 },
            max_errors: 0,
        },
    )
    .await
    .unwrap_or_else(|e| {
        let sql = std::fs::read_to_string(&dest).unwrap();
        panic!(
            "[{}] replay failed: {e}\n{}",
            env.label,
            &sql[..sql.len().min(3000)]
        )
    });
    assert!(imported.errors.is_empty());
    assert_eq!(imported.inserted, 2550, "[{}]", env.label);

    let db = database.as_deref();
    assert_eq!(
        env.table_count(&*dst, db).await,
        env.table_count(&*env.src, env.src_database()).await
    );
    assert_eq!(env.table_count(&*dst, db).await, "2");
    assert_eq!(env.foreign_key_count(&*dst, db).await, "1");
    for t in ["exp_a", "exp_b"] {
        let want = env.fingerprint(&*env.src, env.src_database(), t).await;
        let got = env.fingerprint(&*dst, db, t).await;
        assert_eq!(got, want, "[{}] checksum of {t}", env.label);
    }
    assert!(want_rows(&env.fingerprint(&*dst, db, "exp_a").await, 2500));
}

fn want_rows(fingerprint: &str, rows: u64) -> bool {
    fingerprint.starts_with(&format!("{rows}:"))
}

/// XLSX: sheets split when the (lowered) row limit is exceeded, dictionary sheet present.
pub async fn xlsx_split(env: &Env) {
    use calamine::Reader;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("t.xlsx");
    let report = run_export_tuned(
        env.ctx(&env.src, Recorder::new()),
        export_spec(
            ExportSource::Tables {
                paths: vec![env.table("exp_a"), env.table("exp_b")],
            },
            ExportContent::Both,
            ExportFormat::Xlsx(XlsxOptions::default()),
            &dest,
        ),
        ExportTuning {
            xlsx_sheet_rows: 1000,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] {e}", env.label));
    let path = &report.files[0];
    let mut wb = calamine::open_workbook_auto(path).unwrap();
    assert_eq!(
        wb.sheet_names(),
        [
            "exp_a",
            "exp_a (2)",
            "exp_a (3)",
            "exp_b",
            "Data dictionary"
        ],
        "[{}]",
        env.label
    );
    let mut data_rows = 0;
    for s in ["exp_a", "exp_a (2)", "exp_a (3)"] {
        let r = wb.worksheet_range(s).unwrap();
        data_rows += r.height() - 1;
        assert_eq!(
            r.rows().next().unwrap()[0],
            calamine::Data::String("id".into())
        );
    }
    assert_eq!(data_rows, 2500);
    let first = wb.worksheet_range("exp_a").unwrap();
    assert_eq!(first.get((1, 0)), Some(&calamine::Data::Float(1.0)));
    let dict = wb.worksheet_range("Data dictionary").unwrap();
    assert!(dict.height() > 10, "{}", dict.height());
}

/// CSV zip with BOM and schema.csv; JSON array / NDJSON with schema.json.
pub async fn csv_and_json_files(env: &Env) {
    let dir = tempfile::tempdir().unwrap();
    let source = || ExportSource::Schema { path: env.scope() };

    let csv = run_export(
        env.ctx(&env.src, Recorder::new()),
        export_spec(
            source(),
            ExportContent::Both,
            ExportFormat::Csv(CsvOptions::default()),
            &dir.path().join("c.zip"),
        ),
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] {e}", env.label));
    assert_eq!(
        zip_entry_names(&csv.files[0]),
        ["exp_a.csv", "exp_b.csv", "schema.csv"]
    );
    let exp_b = zip_entry(&csv.files[0], "exp_b.csv");
    assert_eq!(&exp_b[..3], [0xEF, 0xBB, 0xBF]);
    let text = String::from_utf8(exp_b[3..].to_vec()).unwrap();
    assert!(
        text.starts_with("id,a_id,note\r\n1,10,note 1\r\n"),
        "{text}"
    );
    assert_eq!(text.matches("\r\n").count(), 51);

    for (layout, name, ext) in [
        (JsonLayout::Array, "j.zip", "json"),
        (JsonLayout::Ndjson, "n.zip", "ndjson"),
    ] {
        let report = run_export(
            env.ctx(&env.src, Recorder::new()),
            export_spec(
                source(),
                ExportContent::Both,
                ExportFormat::Json(JsonOptions {
                    layout,
                    ..JsonOptions::default()
                }),
                &dir.path().join(name),
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            zip_entry_names(&report.files[0]),
            [
                format!("exp_a.{ext}"),
                format!("exp_b.{ext}"),
                "schema.json".into()
            ]
        );
        let b = zip_entry(&report.files[0], &format!("exp_b.{ext}"));
        let text = String::from_utf8(b).unwrap();
        let rows: Vec<serde_json::Value> = if layout == JsonLayout::Array {
            serde_json::from_str(&text).unwrap()
        } else {
            text.lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        };
        assert_eq!(rows.len(), 50);
        assert_eq!(rows[0]["note"], "note 1");
        let schema: serde_json::Value =
            serde_json::from_slice(&zip_entry(&report.files[0], "schema.json")).unwrap();
        assert_eq!(schema["tables"].as_array().unwrap().len(), 2);
        assert_eq!(schema["tables"][0]["primaryKey"][0], "id");
    }
}

/// Export from a query works for every format.
pub async fn query_export(env: &Env) {
    let dir = tempfile::tempdir().unwrap();
    let text =
        "SELECT b.id, b.note, a.qty FROM exp_b b JOIN exp_a a ON a.id = b.a_id ORDER BY b.id";
    for (name, format) in [
        ("q.csv", ExportFormat::Csv(CsvOptions::default())),
        ("q.json", ExportFormat::Json(JsonOptions::default())),
        ("q.xlsx", ExportFormat::Xlsx(XlsxOptions::default())),
        ("q.sql", ExportFormat::Sql(SqlOptions::default())),
    ] {
        let report = run_export(
            env.ctx(&env.src, Recorder::new()),
            export_spec(
                ExportSource::Query {
                    text: text.into(),
                    database: env.src_database().map(str::to_string),
                },
                ExportContent::Data,
                format,
                &dir.path().join(name),
            ),
        )
        .await
        .unwrap_or_else(|e| panic!("[{}] {name}: {e}", env.label));
        assert_eq!(report.rows, 50, "[{}] {name}", env.label);
    }
    let csv = std::fs::read(dir.path().join("q.csv")).unwrap();
    assert!(
        String::from_utf8_lossy(&csv[3..]).starts_with("id,note,qty\r\n1,note 1,10\r\n"),
        "{}",
        String::from_utf8_lossy(&csv)
    );
}

/// Write `rows` lines `id;label;qty;extra` to a CSV file.
pub fn write_big_csv(path: &Path, rows: u64, bad_line: Option<u64>) {
    let mut w = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    writeln!(w, "ident;label;qty;extra").unwrap();
    for i in 1..=rows {
        // Line `i + 1` of the file (the header is line 1).
        if Some(i + 1) == bad_line {
            writeln!(w, "{i};row {i};not-a-number;x").unwrap();
        } else {
            writeln!(w, "{i};row {i} \"q\";{};x", i % 1000).unwrap();
        }
    }
    w.flush().unwrap();
}

pub async fn create_import_table(env: &Env, session: &dyn Session, database: Option<&str>) {
    let ddl = match env.flavor {
        Flavor::Pg => "CREATE TABLE imp (id bigint PRIMARY KEY, label text, qty integer)",
        Flavor::My => "CREATE TABLE imp (id BIGINT PRIMARY KEY, label TEXT, qty INT)",
    };
    run(session, database, ddl).await;
}

fn dst_db_name(env: &Env, tag: &str, database: &Option<String>) -> String {
    database
        .clone()
        .unwrap_or_else(|| format!("exp_it_dst_{}_{tag}", env.label.replace('-', "_")))
}

pub fn import_table_path(env: &Env, database: &str) -> ObjectPath {
    let db = ObjectPath::root().child(ObjectKind::Database, database);
    let scope = match env.flavor {
        Flavor::Pg => db
            .child(ObjectKind::Folder, "schemas")
            .child(ObjectKind::Schema, "public"),
        Flavor::My => db,
    };
    scope
        .child(ObjectKind::Folder, "tables")
        .child(ObjectKind::Table, "imp")
}

pub fn csv_import_spec(file: &Path, target: ObjectPath, max_errors: u32, batch: u32) -> ImportSpec {
    ImportSpec {
        connection_id: Uuid::nil(),
        file: file.to_string_lossy().into_owned(),
        format: ImportFormat::Csv {
            delimiter: ';',
            quote: '"',
            header: true,
            encoding: TextEncoding::Utf8,
            null_text: None,
        },
        target: ImportTarget::Existing { table: target },
        mapping: vec![
            dbstudio_export::ColumnMapping {
                source: "ident".into(),
                target: "id".into(),
                transform: dbstudio_export::Transform::Trim,
            },
            dbstudio_export::ColumnMapping {
                source: "#2".into(),
                target: "label".into(),
                transform: dbstudio_export::Transform::None,
            },
            dbstudio_export::ColumnMapping {
                source: "qty".into(),
                target: "qty".into(),
                transform: dbstudio_export::Transform::None,
            },
        ],
        on_conflict: None,
        mode: ImportMode::Batches { rows: batch },
        max_errors,
    }
}

/// Import of a CSV file with mapping; a bad value is reported with its line.
pub async fn csv_import(env: &Env, rows: u64) {
    let dir = tempfile::tempdir().unwrap();
    let (dst, database) = env.destination("imp").await;
    let db = database.as_deref();
    let db_name = dst_db_name(env, "imp", &database);
    create_import_table(env, &*dst, db).await;
    let target = import_table_path(env, &db_name);

    let file = dir.path().join("big.csv");
    write_big_csv(&file, rows, None);
    let started = std::time::Instant::now();
    let report = run_import(
        env.ctx(&dst, Recorder::new()),
        csv_import_spec(&file, target.clone(), 0, 5000),
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] import failed: {e}", env.label));
    eprintln!(
        "[{}] imported {rows} rows in {:?}",
        env.label,
        started.elapsed()
    );
    assert_eq!(report.rows_read, rows);
    assert_eq!(report.inserted, rows);
    assert_eq!(
        text_of(&*dst, db, "SELECT COUNT(*) FROM imp").await,
        rows.to_string()
    );
    assert_eq!(
        text_of(
            &*dst,
            db,
            &format!("SELECT label FROM imp WHERE id = {rows}")
        )
        .await,
        format!("row {rows} \"q\"")
    );

    // A bad value at line 4,321 (stop at the first error): the message names the line and
    // nothing of the failing batch is written.
    run(&*dst, db, "DELETE FROM imp").await;
    let bad = dir.path().join("bad.csv");
    write_big_csv(&bad, 6000, Some(4321));
    let err = run_import(
        env.ctx(&dst, Recorder::new()),
        csv_import_spec(&bad, target.clone(), 0, 1000),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, dbstudio_core::Error::Query { message, .. } if message.starts_with("line 4321:")),
        "[{}] {err:?}",
        env.label
    );

    // Tolerating errors: the bad line is listed, everything else arrives.
    run(&*dst, db, "DELETE FROM imp").await;
    let report = run_import(
        env.ctx(&dst, Recorder::new()),
        csv_import_spec(&bad, target, 10, 1000),
    )
    .await
    .unwrap();
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].line, 4321);
    assert_eq!(report.inserted, 5999);
    assert_eq!(text_of(&*dst, db, "SELECT COUNT(*) FROM imp").await, "5999");
}

/// ON CONFLICT handling on a table with data.
pub async fn import_conflicts(env: &Env) {
    let dir = tempfile::tempdir().unwrap();
    let (dst, database) = env.destination("conf").await;
    let db = database.as_deref();
    let db_name = dst_db_name(env, "conf", &database);
    create_import_table(env, &*dst, db).await;
    run(
        &*dst,
        db,
        "INSERT INTO imp VALUES (1, 'old', 1), (2, 'old', 2)",
    )
    .await;
    let file = dir.path().join("c.csv");
    write_big_csv(&file, 4, None);
    let target = import_table_path(env, &db_name);

    let mut s = csv_import_spec(&file, target.clone(), 0, 100);
    s.on_conflict = Some(dbstudio_core::OnConflict::Skip);
    let r = run_import(env.ctx(&dst, Recorder::new()), s).await.unwrap();
    assert_eq!((r.inserted, r.skipped), (2, 2), "[{}]", env.label);
    assert_eq!(
        text_of(&*dst, db, "SELECT label FROM imp WHERE id = 1").await,
        "old"
    );

    // Back to rows 1 and 2 (the skip run added 3 and 4).
    run(&*dst, db, "DELETE FROM imp WHERE id > 2").await;
    let mut s = csv_import_spec(&file, target.clone(), 0, 100);
    s.on_conflict = Some(dbstudio_core::OnConflict::Update);
    let r = run_import(env.ctx(&dst, Recorder::new()), s).await.unwrap();
    // Two rows are new, two exist and change: told apart on both servers.
    assert_eq!(
        (r.inserted, r.updated, r.skipped),
        (2, 2, 0),
        "[{}]",
        env.label
    );
    assert_eq!(
        text_of(&*dst, db, "SELECT label FROM imp WHERE id = 1").await,
        "row 1 \"q\""
    );

    // Without a conflict policy a duplicate key stops the import with the line.
    let err = run_import(
        env.ctx(&dst, Recorder::new()),
        csv_import_spec(&file, target, 0, 100),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, dbstudio_core::Error::Query { message, .. } if message.starts_with("line 2:")),
        "[{}] {err:?}",
        env.label
    );
}

/// CSV export imported back gives the same rows (typed values survive the text round trip).
pub async fn csv_round_trip(env: &Env) {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("b.csv");
    run_export(
        env.ctx(&env.src, Recorder::new()),
        export_spec(
            ExportSource::Tables {
                paths: vec![env.table("exp_b")],
            },
            ExportContent::Data,
            ExportFormat::Csv(CsvOptions::default()),
            &dest,
        ),
    )
    .await
    .unwrap();
    let (dst, database) = env.destination("rt").await;
    let db = database.as_deref();
    let ddl = match env.flavor {
        Flavor::Pg => {
            "CREATE TABLE exp_b (id integer PRIMARY KEY, a_id bigint NOT NULL, note text)"
        }
        Flavor::My => "CREATE TABLE exp_b (id INT PRIMARY KEY, a_id BIGINT NOT NULL, note TEXT)",
    };
    run(&*dst, db, ddl).await;
    let db_name = dst_db_name(env, "rt", &database);
    let table = import_table_path(env, &db_name);
    let table = ObjectPath {
        segments: {
            let mut s = table.segments;
            s.last_mut().unwrap().name = "exp_b".into();
            s
        },
    };
    let report = run_import(
        env.ctx(&dst, Recorder::new()),
        ImportSpec {
            connection_id: Uuid::nil(),
            file: dest.to_string_lossy().into_owned(),
            format: ImportFormat::Csv {
                delimiter: ',',
                quote: '"',
                header: true,
                encoding: TextEncoding::Utf8Bom,
                null_text: None,
            },
            target: ImportTarget::Existing { table },
            mapping: vec![],
            on_conflict: None,
            mode: ImportMode::Batches { rows: 20 },
            max_errors: 0,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] {e}", env.label));
    assert_eq!(report.inserted, 50);
    assert_eq!(
        env.fingerprint(&*dst, db, "exp_b").await,
        env.fingerprint(&*env.src, env.src_database(), "exp_b")
            .await
    );
}

/// CSV with a NULL text (`NULL`, `\N`): NULL, the empty string, the text `NULL` and the text
/// `\N` all come back as what they were.
pub async fn csv_null_text(env: &Env) {
    let dir = tempfile::tempdir().unwrap();
    let s = &*env.src;
    let db = env.src_database();
    run(s, db, "DROP TABLE IF EXISTS exp_n").await;
    run(s, db, "CREATE TABLE exp_n (id INT PRIMARY KEY, v TEXT)").await;
    let lit = |v: &str| match env.flavor {
        Flavor::Pg => format!("E'{}'", v.replace('\\', "\\\\")),
        Flavor::My => format!("'{}'", v.replace('\\', "\\\\")),
    };
    run(
        s,
        db,
        &format!(
            "INSERT INTO exp_n VALUES (1, NULL), (2, ''), (3, 'NULL'), (4, {}), (5, 'x')",
            lit("\\N")
        ),
    )
    .await;
    let agg = match env.flavor {
        Flavor::Pg => "SELECT string_agg(coalesce(v, '~null~'), '|' ORDER BY id) FROM exp_n",
        Flavor::My => {
            "SELECT GROUP_CONCAT(IFNULL(v, '~null~') ORDER BY id SEPARATOR '|') FROM exp_n"
        }
    };
    let want = text_of(s, db, agg).await;
    assert_eq!(want, "~null~||NULL|\\N|x", "fixture");
    for (n, null_text) in ["NULL", "\\N"].into_iter().enumerate() {
        let dest = dir.path().join(format!("n{n}.csv"));
        run_export(
            env.ctx(&env.src, Recorder::new()),
            export_spec(
                ExportSource::Tables {
                    paths: vec![env.table("exp_n")],
                },
                ExportContent::Data,
                ExportFormat::Csv(CsvOptions {
                    null_text: null_text.into(),
                    ..CsvOptions::default()
                }),
                &dest,
            ),
        )
        .await
        .unwrap_or_else(|e| panic!("[{}] {e}", env.label));
        let (dst, database) = env.destination(&format!("nt{n}")).await;
        let db2 = database.as_deref();
        run(
            &*dst,
            db2,
            "CREATE TABLE exp_n (id INT PRIMARY KEY, v TEXT)",
        )
        .await;
        let db_name = dst_db_name(env, &format!("nt{n}"), &database);
        let mut table = import_table_path(env, &db_name);
        table.segments.last_mut().unwrap().name = "exp_n".into();
        let report = run_import(
            env.ctx(&dst, Recorder::new()),
            ImportSpec {
                connection_id: Uuid::nil(),
                file: dest.to_string_lossy().into_owned(),
                format: ImportFormat::Csv {
                    delimiter: ',',
                    quote: '"',
                    header: true,
                    encoding: TextEncoding::Utf8Bom,
                    null_text: Some(null_text.into()),
                },
                target: ImportTarget::Existing { table },
                mapping: vec![],
                on_conflict: None,
                mode: ImportMode::Batches { rows: 10 },
                max_errors: 0,
            },
        )
        .await
        .unwrap_or_else(|e| panic!("[{}] {e}", env.label));
        assert_eq!(report.inserted, 5);
        assert_eq!(
            text_of(&*dst, db2, agg).await,
            want,
            "[{}] null text {null_text}",
            env.label
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Big table (DBSTUDIO_IT_BIG=1)
// ---------------------------------------------------------------------------------------------

pub fn big_enabled() -> bool {
    enabled() && std::env::var("DBSTUDIO_IT_BIG").is_ok_and(|v| v == "1")
}

pub fn big_rows() -> u64 {
    std::env::var("DBSTUDIO_IT_BIG_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000_000)
}

/// Resident set size of this process in KiB.
pub fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

async fn create_big_table(env: &Env, rows: u64) {
    let s = &*env.src;
    let db = env.src_database();
    run(s, db, "DROP TABLE IF EXISTS exp_big").await;
    match env.flavor {
        Flavor::Pg => {
            run(
                s,
                db,
                "CREATE TABLE exp_big (id bigint PRIMARY KEY, a text, b text, n integer)",
            )
            .await;
            run(
                s,
                db,
                &format!(
                    "INSERT INTO exp_big SELECT g, md5(g::text), repeat('x', 40) || g, g % 1000 FROM generate_series(1, {rows}) g"
                ),
            )
            .await;
        }
        Flavor::My => {
            run(s, db, "CREATE TABLE exp_big (id BIGINT PRIMARY KEY, a VARCHAR(40), b VARCHAR(100), n INT)").await;
            run(
                s,
                db,
                "INSERT INTO exp_big SELECT x.n + y.n * 10 + z.n * 100 + 1, MD5(x.n + y.n * 10 + z.n * 100), 'x', 0 FROM (SELECT 0 n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) x, (SELECT 0 n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) y, (SELECT 0 n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) z",
            )
            .await;
            // Double until there are enough rows.
            let mut have: u64 = 1000;
            while have < rows {
                run(
                    s,
                    db,
                    &format!("INSERT INTO exp_big SELECT id + {have}, MD5(id + {have}), CONCAT(REPEAT('x', 40), id + {have}), (id + {have}) % 1000 FROM exp_big"),
                )
                .await;
                have *= 2;
            }
            run(s, db, &format!("DELETE FROM exp_big WHERE id > {rows}")).await;
        }
    }
}

/// Memory stays flat while a huge table is exported, and cancelling leaves no file.
pub async fn big_export(env: &Env) {
    let rows = big_rows();
    create_big_table(env, rows).await;
    let dir = tempfile::tempdir().unwrap();
    let source = || ExportSource::Tables {
        paths: vec![env.table("exp_big")],
    };

    // Full export, sampling the resident memory on the way.
    let reporter = Recorder::new();
    let samples = Arc::new(std::sync::Mutex::new(Vec::<(u64, u64)>::new()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let (samples, stop, reporter) = (samples.clone(), stop.clone(), reporter.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let done = reporter
                    .seen
                    .lock()
                    .unwrap()
                    .last()
                    .map_or(0, |p| p.rows_done);
                samples.lock().unwrap().push((done, rss_kib()));
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        })
    };
    let dest = dir.path().join("big.csv");
    let started = std::time::Instant::now();
    let report = run_export(
        env.ctx(&env.src, reporter.clone()),
        export_spec(
            source(),
            ExportContent::Data,
            ExportFormat::Csv(CsvOptions::default()),
            &dest,
        ),
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] big export failed: {e}", env.label));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    sampler.join().unwrap();
    eprintln!(
        "[{}] exported {} rows ({} MiB) in {:?}",
        env.label,
        report.rows,
        report.bytes / 1_048_576,
        started.elapsed()
    );
    assert_eq!(report.rows, rows);
    let samples = samples.lock().unwrap().clone();
    let early = samples
        .iter()
        .find(|(done, _)| *done >= rows / 10)
        .map_or(0, |s| s.1);
    let peak = samples.iter().map(|s| s.1).max().unwrap_or(0);
    eprintln!(
        "[{}] rss at 10%: {} MiB, peak: {} MiB",
        env.label,
        early / 1024,
        peak / 1024
    );
    assert!(
        peak.saturating_sub(early) < 96 * 1024,
        "[{}] memory grew with the data: {} MiB -> {} MiB",
        env.label,
        early / 1024,
        peak / 1024
    );

    // Cancel at about a quarter of the rows: no file, no temporary file.
    let reporter = Recorder::new();
    let token = reporter.token.clone();
    let watcher = {
        let reporter = reporter.clone();
        tokio::spawn(async move {
            loop {
                let done = reporter
                    .seen
                    .lock()
                    .unwrap()
                    .last()
                    .map_or(0, |p| p.rows_done);
                if done >= rows / 4 {
                    token.cancel();
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
    };
    let cancelled = dir.path().join("cancelled.csv");
    let err = run_export(
        env.ctx(&env.src, reporter),
        export_spec(
            source(),
            ExportContent::Data,
            ExportFormat::Csv(CsvOptions::default()),
            &cancelled,
        ),
    )
    .await
    .unwrap_err();
    watcher.abort();
    assert!(matches!(err, dbstudio_core::Error::Cancelled), "{err:?}");
    let left: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        left,
        ["big.csv"],
        "[{}] cancelled export left files",
        env.label
    );
    run(
        &*env.src,
        env.src_database(),
        "DROP TABLE IF EXISTS exp_big",
    )
    .await;
}

// ---------------------------------------------------------------------------------------------
// Review scenarios
// ---------------------------------------------------------------------------------------------

/// A session whose login database is not the database of the objects: export and import must
/// work in the database the paths name (a PostgreSQL connection is bound to one database).
pub async fn cross_database(env: &Env) {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("b.csv");
    // `admin` is logged in to `dbstudio` (PostgreSQL), the tables live in the fixture database.
    let report = run_export(
        env.ctx(&env.admin, Recorder::new()),
        export_spec(
            ExportSource::Tables {
                paths: vec![env.table("exp_b")],
            },
            ExportContent::Data,
            ExportFormat::Csv(CsvOptions::default()),
            &dest,
        ),
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] export from another database failed: {e}", env.label));
    assert_eq!(report.rows, 50, "[{}]", env.label);
    let query = run_export(
        env.ctx(&env.admin, Recorder::new()),
        export_spec(
            ExportSource::Query {
                text: "SELECT count(*) AS n FROM exp_b".into(),
                database: Some(env.src_db.clone()),
            },
            ExportContent::Data,
            ExportFormat::Json(JsonOptions::default()),
            &dir.path().join("q.json"),
        ),
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] query in another database failed: {e}", env.label));
    assert_eq!(query.rows, 1);

    // Import into a table of yet another database through the same session.
    let (_dst_session, database) = env.destination("xdb").await;
    let db_name = dst_db_name(env, "xdb", &database);
    let ddl = match env.flavor {
        Flavor::Pg => {
            "CREATE TABLE exp_b (id integer PRIMARY KEY, a_id bigint NOT NULL, note text)"
        }
        Flavor::My => "CREATE TABLE exp_b (id INT PRIMARY KEY, a_id BIGINT NOT NULL, note TEXT)",
    };
    run(&*_dst_session, database.as_deref(), ddl).await;
    let table = ObjectPath {
        segments: {
            let mut segs = import_table_path(env, &db_name).segments;
            segs.last_mut().unwrap().name = "exp_b".into();
            segs
        },
    };
    let report = run_import(
        env.ctx(&env.admin, Recorder::new()),
        ImportSpec {
            connection_id: Uuid::nil(),
            file: dest.to_string_lossy().into_owned(),
            format: ImportFormat::Csv {
                delimiter: ',',
                quote: '"',
                header: true,
                encoding: TextEncoding::Utf8Bom,
                null_text: None,
            },
            target: ImportTarget::Existing { table },
            mapping: vec![],
            on_conflict: None,
            mode: ImportMode::Batches { rows: 20 },
            max_errors: 0,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] import into another database failed: {e}", env.label));
    assert_eq!(report.inserted, 50);
    assert_eq!(
        env.fingerprint(&*_dst_session, database.as_deref(), "exp_b")
            .await,
        env.fingerprint(&*env.src, env.src_database(), "exp_b")
            .await
    );
}

/// The lease's own table stream and bulk insert: same connection, same transaction.
pub async fn lease_hooks(env: &Env) {
    use dbstudio_core::{
        BulkInsertRequest, CollectingSink, EditValue, IsolationLevel, LeaseOptions, OnConflict,
        TableStreamRequest, TxOptions,
    };
    let s = &*env.src;
    let db = env.src_database();
    run(s, db, "DROP TABLE IF EXISTS exp_snap").await;
    run(
        s,
        db,
        "CREATE TABLE exp_snap (id BIGINT PRIMARY KEY, label VARCHAR(50))",
    )
    .await;
    run(
        s,
        db,
        "INSERT INTO exp_snap SELECT g, CONCAT('l', g) FROM (SELECT 1 AS g UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9 UNION SELECT 10) t",
    )
    .await;
    let path = env.table("exp_snap");
    let lease = env
        .src
        .open_lease(LeaseOptions {
            database: db.map(str::to_string),
            ..LeaseOptions::default()
        })
        .await
        .unwrap();
    lease
        .begin(TxOptions {
            read_only: true,
            isolation: Some(IsolationLevel::RepeatableRead),
        })
        .await
        .unwrap();
    let stream = |order: bool| TableStreamRequest {
        query_id: Uuid::new_v4(),
        table: path.clone(),
        columns: vec![],
        filters: vec![],
        raw_where: None,
        order_by_key: order,
        batch_size: Some(3),
    };
    let rows_of = |sink: CollectingSink| {
        let mut r = sink.into_results();
        match r.remove(0).outcome {
            dbstudio_core::StatementOutcome::Rows(rs) => rs.rows,
            other => panic!("{other:?}"),
        }
    };
    let mut sink = CollectingSink::new();
    lease
        .stream_table(stream(true), &mut sink)
        .await
        .unwrap_or_else(|e| panic!("[{}] lease.stream_table: {e}", env.label));
    let first = rows_of(sink);
    assert_eq!(first.len(), 10);
    assert_eq!(first[0][0], Value::Int(1), "ordered by the key");
    // Another connection changes the table; the lease still sees its snapshot.
    run(s, db, "DELETE FROM exp_snap WHERE id > 5").await;
    let mut sink = CollectingSink::new();
    lease.stream_table(stream(false), &mut sink).await.unwrap();
    assert_eq!(rows_of(sink).len(), 10, "[{}] snapshot is kept", env.label);
    lease.rollback().await.unwrap();
    let mut sink = CollectingSink::new();
    lease.stream_table(stream(false), &mut sink).await.unwrap();
    assert_eq!(rows_of(sink).len(), 5);

    // Bulk insert runs inside the lease's transaction: a rollback removes it.
    lease.begin(TxOptions::default()).await.unwrap();
    let rows = |from: i64| -> Vec<Vec<EditValue>> {
        (from..from + 100)
            .map(|i| {
                vec![
                    EditValue::Value(Value::Int(i)),
                    EditValue::Text(format!("n{i}")),
                ]
            })
            .collect()
    };
    let request = |rows| BulkInsertRequest {
        table: path.clone(),
        columns: vec!["id".into(), "label".into()],
        rows,
        on_conflict: OnConflict::Error,
        conflict_columns: vec![],
    };
    let r = lease.bulk_insert(request(rows(100))).await.unwrap();
    assert_eq!(r.inserted, 100, "[{}]", env.label);
    let mut sink = CollectingSink::new();
    lease.stream_table(stream(false), &mut sink).await.unwrap();
    assert_eq!(rows_of(sink).len(), 105, "visible inside the transaction");
    assert_eq!(
        text_of(s, db, "SELECT COUNT(*) FROM exp_snap").await,
        "5",
        "not outside it"
    );
    lease.rollback().await.unwrap();
    assert_eq!(text_of(s, db, "SELECT COUNT(*) FROM exp_snap").await, "5");
    // A duplicate key fails the whole call.
    lease.begin(TxOptions::default()).await.unwrap();
    assert!(lease.bulk_insert(request(rows(3))).await.is_err());
    lease.rollback().await.unwrap();
    lease.close().await.unwrap();
    run(s, db, "DROP TABLE exp_snap").await;
}

/// A failing import in one transaction leaves nothing behind, COPY / bulk inserts included.
pub async fn single_transaction_rolls_back(env: &Env) {
    let dir = tempfile::tempdir().unwrap();
    let (dst, database) = env.destination("single").await;
    let db = database.as_deref();
    let db_name = dst_db_name(env, "single", &database);
    create_import_table(env, &*dst, db).await;
    let target = import_table_path(env, &db_name);

    let bad = dir.path().join("bad.csv");
    write_big_csv(&bad, 6000, Some(4321));
    let mut spec = csv_import_spec(&bad, target.clone(), 0, 1000);
    spec.mode = ImportMode::SingleTransaction;
    let err = run_import(env.ctx(&dst, Recorder::new()), spec)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, dbstudio_core::Error::Query { message, .. } if message.starts_with("line 4321:")),
        "[{}] {err:?}",
        env.label
    );
    assert_eq!(
        text_of(&*dst, db, "SELECT COUNT(*) FROM imp").await,
        "0",
        "[{}] the rows of the earlier statements are rolled back too",
        env.label
    );

    let good = dir.path().join("good.csv");
    write_big_csv(&good, 6000, None);
    let mut spec = csv_import_spec(&good, target, 0, 1000);
    spec.mode = ImportMode::SingleTransaction;
    let report = run_import(env.ctx(&dst, Recorder::new()), spec)
        .await
        .unwrap();
    assert_eq!(report.inserted, 6000);
    assert_eq!(text_of(&*dst, db, "SELECT COUNT(*) FROM imp").await, "6000");
}

fn pg_columns_without_tags() -> &'static str {
    "id bigint PRIMARY KEY, name text, qty integer NOT NULL DEFAULT 0, price numeric(12,2), ratio double precision, flag boolean, born date, created timestamp, seen timestamptz, blob bytea, doc jsonb, uid uuid"
}

async fn typed_fingerprint(env: &Env, session: &dyn Session, db: Option<&str>) -> String {
    match env.flavor {
        Flavor::Pg => text_of(
            session,
            db,
            "SELECT count(*)::text || ':' || coalesce(md5(string_agg(ROW(id, name, qty, price, ratio, flag, born, created, seen, blob, doc, uid)::text, '|' ORDER BY id)), '') FROM exp_a",
        )
        .await,
        Flavor::My => env.fingerprint(session, db, "exp_a").await,
    }
}

/// Typed columns (bytea / blob, decimals, json, booleans, dates) come back identical after a
/// CSV, JSON or XLSX round trip.
pub async fn typed_round_trips(env: &Env) {
    let dir = tempfile::tempdir().unwrap();
    let (dst, database) = env.destination("typed").await;
    let db = database.as_deref();
    let db_name = dst_db_name(env, "typed", &database);
    let ddl = match env.flavor {
        Flavor::Pg => format!("CREATE TABLE exp_a ({})", pg_columns_without_tags()),
        Flavor::My => format!("CREATE TABLE exp_a LIKE {}.exp_a", env.src_db),
    };
    run(&*dst, db, &ddl).await;
    let table = ObjectPath {
        segments: {
            let mut segs = import_table_path(env, &db_name).segments;
            segs.last_mut().unwrap().name = "exp_a".into();
            segs
        },
    };
    let want = typed_fingerprint(env, &*env.src, env.src_database()).await;
    let cases: [(&str, ExportFormat, ImportFormat); 3] = [
        (
            "t.csv",
            ExportFormat::Csv(CsvOptions::default()),
            ImportFormat::Csv {
                delimiter: ',',
                quote: '"',
                header: true,
                encoding: TextEncoding::Utf8Bom,
                null_text: None,
            },
        ),
        (
            "t.json",
            ExportFormat::Json(JsonOptions::default()),
            ImportFormat::Json {
                layout: JsonLayout::Array,
            },
        ),
        (
            "t.xlsx",
            ExportFormat::Xlsx(XlsxOptions::default()),
            ImportFormat::Xlsx {
                sheet: String::new(),
                header: true,
            },
        ),
    ];
    for (name, export, import) in cases {
        run(&*dst, db, "DELETE FROM exp_a").await;
        let file = dir.path().join(name);
        run_export(
            env.ctx(&env.src, Recorder::new()),
            export_spec(
                ExportSource::Tables {
                    paths: vec![env.table("exp_a")],
                },
                ExportContent::Data,
                export,
                &file,
            ),
        )
        .await
        .unwrap_or_else(|e| panic!("[{}] {name}: export: {e}", env.label));
        let report = run_import(
            env.ctx(&dst, Recorder::new()),
            ImportSpec {
                connection_id: Uuid::nil(),
                file: file.to_string_lossy().into_owned(),
                format: import,
                target: ImportTarget::Existing {
                    table: table.clone(),
                },
                mapping: vec![],
                on_conflict: None,
                mode: ImportMode::Batches { rows: 700 },
                max_errors: 0,
            },
        )
        .await
        .unwrap_or_else(|e| panic!("[{}] {name}: import: {e}", env.label));
        assert_eq!(report.inserted, 2500, "[{}] {name}", env.label);
        assert_eq!(
            typed_fingerprint(env, &*dst, db).await,
            want,
            "[{}] {name} round trip changed values",
            env.label
        );
    }
}

/// A value that looks like an injection survives the SQL script, and the destination's modes
/// do not matter.
pub async fn hostile_values(env: &Env) {
    let dir = tempfile::tempdir().unwrap();
    let s = &*env.src;
    let db = env.src_database();
    run(s, db, "DROP TABLE IF EXISTS exp_h").await;
    run(s, db, "CREATE TABLE exp_h (id INT PRIMARY KEY, t TEXT)").await;
    let hostile = "\\', 0); DROP TABLE users; -- ";
    let lit = |v: &str| match env.flavor {
        Flavor::Pg => format!("E'{}'", v.replace('\\', "\\\\").replace('\'', "''")),
        Flavor::My => format!("'{}'", v.replace('\\', "\\\\").replace('\'', "''")),
    };
    run(
        s,
        db,
        &format!(
            "INSERT INTO exp_h VALUES (1, {}), (2, {}), (3, {})",
            lit(hostile),
            lit("plain \\ back\\slash"),
            lit("new\nline\ttab")
        ),
    )
    .await;
    let dest = dir.path().join("h.sql");
    run_export(
        env.ctx(&env.src, Recorder::new()),
        export_spec(
            ExportSource::Tables {
                paths: vec![env.table("exp_h")],
            },
            ExportContent::Both,
            ExportFormat::Sql(sql_options_unqualified()),
            &dest,
        ),
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] {e}", env.label));

    let (dst, database) = env.destination("hostile").await;
    let db2 = database.as_deref();
    run(&*dst, db2, "CREATE TABLE users (id INT PRIMARY KEY)").await;
    run(&*dst, db2, "INSERT INTO users VALUES (1)").await;
    run_import(
        env.ctx(&dst, Recorder::new()),
        ImportSpec {
            connection_id: Uuid::nil(),
            file: dest.to_string_lossy().into_owned(),
            format: ImportFormat::Sql {
                continue_on_error: false,
            },
            target: ImportTarget::Script {
                database: database.clone(),
            },
            mapping: vec![],
            on_conflict: None,
            mode: ImportMode::Batches { rows: 100 },
            max_errors: 0,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] replay failed: {e}", env.label));
    assert_eq!(text_of(&*dst, db2, "SELECT COUNT(*) FROM users").await, "1");
    assert_eq!(
        text_of(&*dst, db2, "SELECT t FROM exp_h WHERE id = 1").await,
        hostile
    );
    assert_eq!(
        text_of(&*dst, db2, "SELECT t FROM exp_h WHERE id = 2").await,
        "plain \\ back\\slash"
    );
    assert_eq!(
        text_of(&*dst, db2, "SELECT t FROM exp_h WHERE id = 3").await,
        "new\nline\ttab"
    );
    run(s, db, "DROP TABLE exp_h").await;
}
