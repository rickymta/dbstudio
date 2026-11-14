//! Export / import against Redis 7 (TLS), Valkey 8 and MongoDB 7 / 4.4 (only with
//! `DBSTUDIO_IT=1`). A server that is not running is skipped unless `DBSTUDIO_IT_STRICT=1`.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use common::Recorder;
use dbstudio_core::{
    ApplyResult, ChangeSet, CollectingSink, ConnectionConfig, DbKind, Driver, ExtValue, KeyChange,
    KeyChangeSet, KeyData, KeyReadRequest, KeyRecord, KeyScanRequest, ObjectKind, ObjectPath,
    OnConflict, PutMode, QueryRequest, Secret, Session, StatementOutcome, TableDataRequest,
    TlsMode, Value, execute_collect,
};
use dbstudio_export::{
    ColumnMapping, CsvOptions, ExportContent, ExportContext, ExportDestination, ExportFormat,
    ExportSource, ExportSpec, ExtendedJson, ImportFormat, ImportMode, ImportSpec, ImportTarget,
    JsonLayout, JsonOptions, TextEncoding, Transform, ValueFormat, XlsxOptions, run_export,
    run_import,
};
use uuid::Uuid;

fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").is_ok_and(|v| v == "1")
}

fn strict() -> bool {
    std::env::var("DBSTUDIO_IT_STRICT").is_ok_and(|v| v == "1")
}

fn port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

// ---------------------------------------------------------------------------------------------
// Redis
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Redis {
    name: &'static str,
    var: &'static str,
    default_port: u16,
    tls: bool,
    /// Database used by the test (the server is shared with other tests).
    db: i64,
}

async fn redis_connect(r: Redis) -> Option<Arc<dyn Session>> {
    let mut cfg = ConnectionConfig::new(DbKind::Redis, r.name, "127.0.0.1");
    cfg.port = port(r.var, r.default_port);
    cfg.username = Some("dbstudio".into());
    cfg.tls.mode = if r.tls {
        TlsMode::Require
    } else {
        TlsMode::Disable
    };
    match dbstudio_driver_redis::RedisDriver::new()
        .connect(&cfg, Some(&Secret::new("dbstudio_test")))
        .await
    {
        Ok(s) => Some(Arc::from(s)),
        Err(e) if strict() => panic!("{} is not reachable: {e}", r.name),
        Err(e) => {
            eprintln!("skipping {}: {e}", r.name);
            None
        }
    }
}

fn db_path(db: i64) -> ObjectPath {
    ObjectPath::root().child(ObjectKind::Database, db.to_string())
}

async fn redis_cmd(s: &dyn Session, db: i64, cmd: &str) {
    let mut req = QueryRequest::new(cmd);
    req.database = Some(db.to_string());
    execute_collect(s, req)
        .await
        .unwrap_or_else(|e| panic!("`{cmd}` failed: {e}"));
}

async fn dbsize(s: &dyn Session, db: i64) -> i64 {
    let mut req = QueryRequest::new("DBSIZE");
    req.database = Some(db.to_string());
    match execute_collect(s, req).await.unwrap().remove(0).outcome {
        StatementOutcome::Rows(rs) => match rs.rows[0].last() {
            Some(Value::Int(i)) => *i,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

async fn put(s: &dyn Session, db: i64, changes: Vec<KeyChange>) -> ApplyResult {
    s.apply_changes(ChangeSet::Keys(KeyChangeSet {
        database: db_path(db),
        changes,
    }))
    .await
    .unwrap()
}

fn redis_ctx(session: Arc<dyn Session>) -> ExportContext {
    ExportContext {
        session,
        kind: DbKind::Redis,
        dialect: dbstudio_core::ScriptDialect::RedisCli,
        reporter: Recorder::new(),
    }
}

/// Every key as `(type, entries, ttl)`, read page by page the way the export does.
async fn dump(s: &dyn Session, db: i64) -> BTreeMap<String, (String, String, Option<i64>)> {
    let mut out = BTreeMap::new();
    let mut cursor = None;
    loop {
        let page = s
            .scan_keys(KeyScanRequest {
                database: db_path(db),
                pattern: None,
                cursor: cursor.clone(),
                count: 100,
                type_filter: None,
            })
            .await
            .unwrap();
        for name in page.keys {
            let mut entries = String::new();
            let mut c = None;
            let mut head: Option<(String, Option<i64>)> = None;
            loop {
                let p = s
                    .read_key(KeyReadRequest {
                        key: db_path(db).child(ObjectKind::Key, &name),
                        cursor: c.clone(),
                        count: 100,
                        pattern: None,
                        max_string_bytes: None,
                    })
                    .await
                    .unwrap();
                head.get_or_insert((p.key_type.clone(), p.ttl_ms));
                let mut e = serde_json::to_value(&p.entries).unwrap();
                // Set / hash scan order is not defined: compare sorted.
                for field in ["members", "fields"] {
                    if let Some(a) = e.get_mut(field).and_then(|v| v.as_array_mut()) {
                        a.sort_by_key(ToString::to_string);
                    }
                }
                entries.push_str(&e.to_string());
                c = p.next_cursor;
                if c.is_none() {
                    break;
                }
            }
            let (ty, ttl) = head.unwrap();
            out.insert(name, (ty, entries, ttl));
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    out
}

async fn redis_round_trip(r: Redis) {
    if !enabled() {
        return;
    }
    let Some(s) = redis_connect(r).await else {
        return;
    };
    let db = r.db;
    redis_cmd(&*s, db, "FLUSHDB").await;
    // The six types, a TTL, a binary key and a large collection (several `read_key` pages).
    redis_cmd(&*s, db, "SET s:plain \"héllo wörld\"").await;
    redis_cmd(&*s, db, "SET s:ttl value PX 600000").await;
    redis_cmd(&*s, db, "HSET h:user name Ann age 30").await;
    redis_cmd(&*s, db, "RPUSH l:list a b c").await;
    redis_cmd(&*s, db, "SADD set:s x y z").await;
    redis_cmd(&*s, db, "ZADD z:z 1.5 m1 2 m2").await;
    redis_cmd(&*s, db, "XADD x:stream 1-1 f v").await;
    redis_cmd(&*s, db, "XADD x:stream 2-1 f w").await;
    let big: Vec<(Value, Value)> = (0..450)
        .map(|i| (text(&format!("f{i}")), text(&format!("v{i}"))))
        .collect();
    put(
        &*s,
        db,
        vec![
            KeyChange::Put {
                key: "\0b64:AJ+Slv8=".into(),
                data: KeyData::String {
                    value: Value::Bytes(vec![0, 159, 146, 150, 255]),
                },
                ttl_ms: None,
                mode: PutMode::Replace,
            },
            KeyChange::Put {
                key: "h:big".into(),
                data: KeyData::Hash { fields: big },
                ttl_ms: None,
                mode: PutMode::Replace,
            },
        ],
    )
    .await;
    let before = dump(&*s, db).await;
    assert_eq!(before.len(), 9, "{:?}", before.keys());

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("keys.ndjson");
    let report = run_export(
        redis_ctx(s.clone()),
        ExportSpec {
            connection_id: Uuid::nil(),
            source: ExportSource::Keys {
                database: db_path(db),
                pattern: "*".into(),
            },
            content: ExportContent::Data,
            format: ExportFormat::Json(JsonOptions {
                layout: JsonLayout::Ndjson,
                ..JsonOptions::default()
            }),
            destination: ExportDestination {
                path: file.to_string_lossy().into_owned(),
                overwrite: false,
            },
            values: ValueFormat::default(),
        },
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] {e}", r.name));
    assert_eq!(report.rows, 9, "[{}] {:?}", r.name, report.warnings);
    let lines = std::fs::read_to_string(&report.files[0]).unwrap();
    let records: Vec<KeyRecord> = lines
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(records.len(), 9);
    assert!(
        records
            .iter()
            .any(|r| r.key == "s:ttl" && r.expire_at_ms.is_some())
    );

    redis_cmd(&*s, db, "FLUSHDB").await;
    assert_eq!(dbsize(&*s, db).await, 0);
    let imported = run_import(
        redis_ctx(s.clone()),
        ImportSpec {
            connection_id: Uuid::nil(),
            file: report.files[0].clone(),
            format: ImportFormat::Json {
                layout: JsonLayout::Ndjson,
            },
            target: ImportTarget::Keys {
                database: db_path(db),
                mode: PutMode::Replace,
            },
            mapping: vec![],
            on_conflict: None,
            mode: ImportMode::Batches { rows: 100 },
            max_errors: 0,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] {e}", r.name));
    assert_eq!(
        (imported.rows_read, imported.inserted),
        (9, 9),
        "{imported:?}"
    );
    let after = dump(&*s, db).await;
    assert_eq!(after.len(), before.len());
    for (key, (ty, entries, ttl)) in &before {
        let (ty2, entries2, ttl2) = &after[key];
        assert_eq!((ty, entries), (ty2, entries2), "[{}] key {key:?}", r.name);
        match (ttl, ttl2) {
            (None, None) => {}
            (Some(a), Some(b)) => assert!((a - b).abs() < 2_000 + 5_000, "ttl {a} vs {b}"),
            other => panic!("ttl changed: {other:?}"),
        }
    }
    // `IfAbsent` keeps what is there.
    redis_cmd(&*s, db, "SET s:plain changed").await;
    let again = run_import(
        redis_ctx(s.clone()),
        ImportSpec {
            connection_id: Uuid::nil(),
            file: report.files[0].clone(),
            format: ImportFormat::Json {
                layout: JsonLayout::Ndjson,
            },
            target: ImportTarget::Keys {
                database: db_path(db),
                mode: PutMode::IfAbsent,
            },
            mapping: vec![],
            on_conflict: None,
            mode: ImportMode::Batches { rows: 100 },
            max_errors: 0,
        },
    )
    .await
    .unwrap();
    assert_eq!(again.skipped, 9, "{again:?}");
    assert!(dump(&*s, db).await["s:plain"].1.contains("changed"));
    redis_cmd(&*s, db, "FLUSHDB").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn redis7_keys_round_trip() {
    redis_round_trip(Redis {
        name: "redis7",
        var: "DBSTUDIO_IT_REDIS7_PORT",
        default_port: 63797,
        tls: true,
        db: 11,
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn valkey8_keys_round_trip() {
    redis_round_trip(Redis {
        name: "valkey8",
        var: "DBSTUDIO_IT_VALKEY8_PORT",
        default_port: 63798,
        tls: false,
        db: 11,
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// MongoDB
// ---------------------------------------------------------------------------------------------

async fn mongo_connect(replica: bool) -> Option<Arc<dyn Session>> {
    let name = if replica { "mongo7" } else { "mongo44" };
    let mut cfg = ConnectionConfig::new(DbKind::MongoDb, name, "127.0.0.1");
    cfg.port = if replica {
        port("DBSTUDIO_IT_MONGO7_PORT", 27077)
    } else {
        port("DBSTUDIO_IT_MONGO44_PORT", 27044)
    };
    cfg.username = Some("dbstudio".into());
    cfg.tls.mode = if replica {
        TlsMode::Prefer
    } else {
        TlsMode::Disable
    };
    cfg.options = BTreeMap::from([
        ("authSource".to_string(), "admin".to_string()),
        ("directConnection".to_string(), "true".to_string()),
    ]);
    match dbstudio_driver_mongodb::MongoDriver::new()
        .connect(&cfg, Some(&Secret::new("dbstudio_test")))
        .await
    {
        Ok(s) => Some(Arc::from(s)),
        Err(e) if strict() => panic!("{name} is not reachable: {e}"),
        Err(e) => {
            eprintln!("skipping {name}: {e}");
            None
        }
    }
}

fn mongo_ctx(session: Arc<dyn Session>) -> ExportContext {
    ExportContext {
        session,
        kind: DbKind::MongoDb,
        dialect: dbstudio_core::ScriptDialect::MongoShell,
        reporter: Recorder::new(),
    }
}

fn coll_path(db: &str, coll: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Database, db)
        .child(ObjectKind::Folder, "collections")
        .child(ObjectKind::Collection, coll)
}

async fn shell(s: &dyn Session, db: &str, text: &str) {
    let mut req = QueryRequest::new(text);
    req.database = Some(db.into());
    execute_collect(s, req)
        .await
        .unwrap_or_else(|e| panic!("{text}: {e}"));
}

/// Documents of a collection sorted by `_id`, as relaxed EJSON text.
async fn documents(s: &dyn Session, db: &str, coll: &str) -> Vec<String> {
    let mut sink = CollectingSink::new();
    s.fetch_table(
        TableDataRequest {
            query_id: Uuid::new_v4(),
            table: coll_path(db, coll),
            filters: vec![],
            raw_where: None,
            sort: vec![],
            offset: 0,
            limit: 10_000,
            batch_size: Some(100),
        },
        &mut sink,
    )
    .await
    .unwrap();
    let mut docs: Vec<String> = match sink.into_results().remove(0).outcome {
        StatementOutcome::Rows(rs) => rs
            .rows
            .iter()
            .map(|r| {
                let mut out = String::new();
                match &r[0] {
                    Value::Document(_) => {
                        dbstudio_export_test_json(&r[0], &mut out);
                    }
                    other => out.push_str(&format!("{other:?}")),
                }
                out
            })
            .collect(),
        other => panic!("{other:?}"),
    };
    docs.sort();
    docs
}

/// Stable text of a document value (field order kept, extension values by `type:text`).
fn dbstudio_export_test_json(v: &Value, out: &mut String) {
    match v {
        Value::Document(fields) => {
            out.push('{');
            for (i, (k, v)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&format!("{k:?}:"));
                dbstudio_export_test_json(v, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                dbstudio_export_test_json(v, out);
            }
            out.push(']');
        }
        Value::Ext(ExtValue { ty, text }) => out.push_str(&format!("<{ty}:{text}>")),
        other => out.push_str(&format!("{other:?}")),
    }
}

fn export_spec(
    db: &str,
    coll: &str,
    format: ExportFormat,
    values: ValueFormat,
    dest: &Path,
) -> ExportSpec {
    ExportSpec {
        connection_id: Uuid::nil(),
        source: ExportSource::Tables {
            paths: vec![coll_path(db, coll)],
        },
        content: ExportContent::Data,
        format,
        destination: ExportDestination {
            path: dest.to_string_lossy().into_owned(),
            overwrite: false,
        },
        values,
    }
}

fn import_spec(file: &Path, format: ImportFormat, target: ImportTarget) -> ImportSpec {
    ImportSpec {
        connection_id: Uuid::nil(),
        file: file.to_string_lossy().into_owned(),
        format,
        target,
        mapping: vec![],
        on_conflict: None,
        mode: ImportMode::Batches { rows: 2 },
        max_errors: 0,
    }
}

/// Drops the test database when the test ends, also when it fails.
struct DropDatabase {
    replica: bool,
    db: String,
}

impl Drop for DropDatabase {
    fn drop(&mut self) {
        let (replica, db) = (self.replica, self.db.clone());
        // A fresh thread and runtime: this may run while the test runtime is shutting down.
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                if let Some(s) = mongo_connect(replica).await {
                    let mut req = QueryRequest::new("db.dropDatabase()");
                    req.database = Some(db);
                    let _ = execute_collect(&*s, req).await;
                }
            });
        })
        .join();
    }
}

async fn mongo_scenarios(replica: bool) {
    if !enabled() {
        return;
    }
    let Some(s) = mongo_connect(replica).await else {
        return;
    };
    let db = format!("dbs_exp_{}", Uuid::new_v4().simple());
    let _guard = DropDatabase {
        replica,
        db: db.clone(),
    };
    mongo_body(s, &db).await;
}

async fn mongo_body(s: Arc<dyn Session>, db: &str) {
    shell(
        &*s,
        db,
        r#"db.people.insertMany([
            {_id: 1, name: "Ann", address: {city: "Hue", geo: {lat: 16.4}}, tags: ["a", "b"], n: NumberInt(5)},
            {_id: 2, name: "Bob", extra: true, items: [{name: "x"}, {name: "y"}]},
            {_id: 3, address: {city: "Hanoi"}, born: ISODate("2020-01-02T03:04:05Z"), big: NumberLong("9007199254740993"), d: NumberDecimal("1.50"), o: ObjectId("507f1f77bcf86cd799439011")}
        ])"#,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();

    // §2.2 CSV: key union of the documents, `_id` first.
    let csv = dir.path().join("people.csv");
    let report = run_export(
        mongo_ctx(s.clone()),
        export_spec(
            db,
            "people",
            ExportFormat::Csv(CsvOptions {
                encoding: TextEncoding::Utf8,
                ..CsvOptions::default()
            }),
            ValueFormat::default(),
            &csv,
        ),
    )
    .await
    .unwrap_or_else(|e| panic!("csv export: {e}"));
    assert_eq!(report.rows, 3);
    let content = std::fs::read_to_string(&report.files[0]).unwrap();
    let header = content.lines().next().unwrap();
    assert!(header.starts_with("_id,"), "{header}");
    for key in [
        "name", "address", "tags", "extra", "items", "born", "big", "d", "o", "n",
    ] {
        assert!(header.split(',').any(|c| c == key), "{key} in {header}");
    }
    assert!(content.contains("{\"\"city\"\":\"\"Hue\"\""), "{content}");
    assert!(content.contains("507f1f77bcf86cd799439011"), "{content}");

    // Configured paths, nested and by array index; a missing path is an empty cell.
    let csv2 = dir.path().join("paths.csv");
    let report = run_export(
        mongo_ctx(s.clone()),
        export_spec(
            db,
            "people",
            ExportFormat::Csv(CsvOptions {
                encoding: TextEncoding::Utf8,
                line_ending: dbstudio_export::LineEnding::Lf,
                ..CsvOptions::default()
            }),
            ValueFormat {
                flatten_fields: vec![
                    "_id".into(),
                    "address.city".into(),
                    "address.geo.lat".into(),
                    "items.1.name".into(),
                    "nope".into(),
                ],
                ..ValueFormat::default()
            },
            &csv2,
        ),
    )
    .await
    .unwrap_or_else(|e| panic!("csv paths export: {e}"));
    let lines: Vec<String> = std::fs::read_to_string(&report.files[0])
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        lines[0],
        "_id,address.city,address.geo.lat,items.1.name,nope"
    );
    let mut rows: Vec<&str> = lines[1..].iter().map(String::as_str).collect();
    rows.sort_unstable();
    assert_eq!(rows, ["1,Hue,16.4,,", "2,,,y,", "3,Hanoi,,,"]);

    // XLSX goes through the same flattening.
    let xlsx = dir.path().join("people.xlsx");
    let report = run_export(
        mongo_ctx(s.clone()),
        export_spec(
            db,
            "people",
            ExportFormat::Xlsx(XlsxOptions::default()),
            ValueFormat {
                flatten_fields: vec!["_id".into(), "name".into(), "address.city".into()],
                ..ValueFormat::default()
            },
            &xlsx,
        ),
    )
    .await
    .unwrap_or_else(|e| panic!("xlsx export: {e}"));
    assert_eq!(report.rows, 3);
    {
        use calamine::{Reader, open_workbook_auto};
        let mut wb = open_workbook_auto(&report.files[0]).unwrap();
        let name = wb.sheet_names()[0].clone();
        let range = wb.worksheet_range(&name).unwrap();
        assert_eq!(range.rows().next().unwrap().len(), 3);
        assert_eq!(range.rows().count(), 4);
    }

    // §2.4 NDJSON (canonical EJSON) into a new collection and back: ObjectId, Decimal128, Date
    // and Int64 survive.
    let nd = dir.path().join("people.ndjson");
    let report = run_export(
        mongo_ctx(s.clone()),
        export_spec(
            db,
            "people",
            ExportFormat::Json(JsonOptions {
                layout: JsonLayout::Ndjson,
                extended_json: ExtendedJson::Canonical,
                ..JsonOptions::default()
            }),
            ValueFormat::default(),
            &nd,
        ),
    )
    .await
    .unwrap_or_else(|e| panic!("ndjson export: {e}"));
    assert_eq!(report.rows, 3);
    let copied = import_spec(
        Path::new(&report.files[0]),
        ImportFormat::Json {
            layout: JsonLayout::Ndjson,
        },
        ImportTarget::New {
            parent: ObjectPath::root().child(ObjectKind::Database, db),
            name: "people_copy".into(),
            columns: vec![],
        },
    );
    let r = run_import(mongo_ctx(s.clone()), copied)
        .await
        .unwrap_or_else(|e| panic!("ndjson import: {e}"));
    assert_eq!((r.rows_read, r.inserted), (3, 3), "{r:?}");
    assert_eq!(
        documents(&*s, db, "people_copy").await,
        documents(&*s, db, "people").await
    );

    // `Skip` counts the duplicates, `Update` replaces by `_id`.
    shell(
        &*s,
        db,
        r#"db.people_copy.updateOne({_id: 1}, {$set: {name: "Changed"}})"#,
    )
    .await;
    let mut skip = import_spec(
        Path::new(&report.files[0]),
        ImportFormat::Json {
            layout: JsonLayout::Ndjson,
        },
        ImportTarget::Existing {
            table: coll_path(db, "people_copy"),
        },
    );
    skip.on_conflict = Some(OnConflict::Skip);
    let r = run_import(mongo_ctx(s.clone()), skip.clone())
        .await
        .unwrap();
    assert_eq!((r.inserted, r.skipped), (0, 3), "{r:?}");
    assert!(
        documents(&*s, db, "people_copy")
            .await
            .iter()
            .any(|d| d.contains("Changed"))
    );
    skip.on_conflict = Some(OnConflict::Update);
    let r = run_import(mongo_ctx(s.clone()), skip.clone())
        .await
        .unwrap();
    assert_eq!(r.updated + r.inserted, 3, "{r:?}");
    assert_eq!(
        documents(&*s, db, "people_copy").await,
        documents(&*s, db, "people").await
    );
    // Error policy: the duplicate is reported with its line and tolerated up to `max_errors`.
    skip.on_conflict = Some(OnConflict::Error);
    let e = run_import(mongo_ctx(s.clone()), skip.clone())
        .await
        .unwrap_err();
    assert!(e.message().contains("line 1"), "{e:?}");
    skip.max_errors = 10;
    let r = run_import(mongo_ctx(s.clone()), skip).await.unwrap();
    assert_eq!((r.inserted, r.errors.len()), (0, 3), "{r:?}");

    // §2.4 CSV with a nested mapping; text cells become typed values.
    let flat = dir.path().join("flat.csv");
    std::fs::write(
        &flat,
        "id;name;city;lat;age;code\n10;Eve;Da Nang;16.05;31;007\n11;Fay;;;;008\n",
    )
    .unwrap();
    let mut csv_import = import_spec(
        &flat,
        ImportFormat::Csv {
            delimiter: ';',
            quote: '"',
            header: true,
            encoding: TextEncoding::Utf8,
            null_text: None,
        },
        ImportTarget::New {
            parent: ObjectPath::root().child(ObjectKind::Database, db),
            name: "from_csv".into(),
            columns: vec![],
        },
    );
    let map = |s: &str, t: &str| ColumnMapping {
        source: s.into(),
        target: t.into(),
        transform: Transform::Trim,
    };
    csv_import.mapping = vec![
        map("id", "_id"),
        map("name", "name"),
        map("city", "address.city"),
        map("lat", "address.geo.lat"),
        map("age", "age"),
        map("code", "code"),
    ];
    let r = run_import(mongo_ctx(s.clone()), csv_import)
        .await
        .unwrap_or_else(|e| panic!("csv import: {e}"));
    assert_eq!((r.rows_read, r.inserted), (2, 2), "{r:?}");
    let docs = documents(&*s, db, "from_csv").await;
    assert_eq!(docs.len(), 2);
    assert!(docs[0].contains("\"city\":Text(\"Da Nang\")"), "{docs:?}");
    assert!(docs[0].contains("\"lat\":Float(16.05)"), "{docs:?}");
    assert!(docs[0].contains("\"age\":Int(31)"), "{docs:?}");
    // A leading-zero code stays text, an empty cell leaves the field out.
    assert!(docs[0].contains("\"code\":Text(\"007\")"), "{docs:?}");
    assert!(!docs[1].contains("address"), "{docs:?}");

    // `Update` from CSV only sets the columns of the file: other fields survive.
    let before = documents(&*s, db, "people").await;
    let status = dir.path().join("status.csv");
    std::fs::write(&status, "_id,status,address.zip\n1,active,100000\n").unwrap();
    let mut upd = import_spec(
        &status,
        ImportFormat::Csv {
            delimiter: ',',
            quote: '"',
            header: true,
            encoding: TextEncoding::Utf8,
            null_text: None,
        },
        ImportTarget::Existing {
            table: coll_path(db, "people"),
        },
    );
    upd.on_conflict = Some(OnConflict::Update);
    let r = run_import(mongo_ctx(s.clone()), upd)
        .await
        .unwrap_or_else(|e| panic!("csv update: {e}"));
    assert_eq!(r.updated, 1, "{r:?}");
    let after = documents(&*s, db, "people").await;
    let doc1 = after
        .iter()
        .find(|d| d.contains("\"name\":Text(\"Ann\")"))
        .unwrap();
    assert!(doc1.contains("\"status\":Text(\"active\")"), "{doc1}");
    // The nested path is set inside the existing sub-document: `city` and `geo` are still there.
    assert!(
        doc1.contains("\"zip\":Int(100000)") && doc1.contains("\"city\":Text(\"Hue\")"),
        "{doc1}"
    );
    assert!(
        doc1.contains("\"tags\":[Text(\"a\"),Text(\"b\")]"),
        "{doc1}"
    );
    assert_eq!(after.len(), before.len());

    // CSV round trip keeps types: ObjectId, Date, Decimal128, Int64, sub-documents, arrays.
    let rt = dir.path().join("rt.csv");
    let report = run_export(
        mongo_ctx(s.clone()),
        export_spec(
            db,
            "people_copy",
            ExportFormat::Csv(CsvOptions {
                encoding: TextEncoding::Utf8,
                ..CsvOptions::default()
            }),
            ValueFormat::default(),
            &rt,
        ),
    )
    .await
    .unwrap();
    let back = import_spec(
        Path::new(&report.files[0]),
        ImportFormat::Csv {
            delimiter: ',',
            quote: '"',
            header: true,
            encoding: TextEncoding::Utf8,
            null_text: None,
        },
        ImportTarget::New {
            parent: ObjectPath::root().child(ObjectKind::Database, db),
            name: "csv_back".into(),
            columns: vec![],
        },
    );
    run_import(mongo_ctx(s.clone()), back)
        .await
        .unwrap_or_else(|e| panic!("csv back: {e}"));
    assert_eq!(
        documents(&*s, db, "csv_back").await,
        documents(&*s, db, "people_copy").await
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mongo7_export_and_import() {
    mongo_scenarios(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mongo44_export_and_import() {
    mongo_scenarios(false).await;
}

// ---------------------------------------------------------------------------------------------
// Large values
// ---------------------------------------------------------------------------------------------

fn digest(v: &Value) -> String {
    use std::hash::{Hash, Hasher};
    fn hash_bytes(b: &[u8]) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        b.hash(&mut h);
        h.finish()
    }
    match v {
        Value::Text(t) => format!("t{}:{}", t.len(), hash_bytes(t.as_bytes())),
        Value::Bytes(b) => format!("b{}:{}", b.len(), hash_bytes(b)),
        Value::Document(f) => f
            .iter()
            .map(|(k, v)| format!("{k}={}", digest(v)))
            .collect::<Vec<_>>()
            .join(","),
        other => format!("{other:?}"),
    }
}

/// A 20 MB string (text and binary) survives the Redis keys export and import.
#[tokio::test(flavor = "multi_thread")]
async fn redis_20mb_strings_round_trip() {
    if !enabled() {
        return;
    }
    let r = Redis {
        name: "valkey8",
        var: "DBSTUDIO_IT_VALKEY8_PORT",
        default_port: 63798,
        tls: false,
        db: 10,
    };
    let Some(s) = redis_connect(r).await else {
        return;
    };
    let db = r.db;
    redis_cmd(&*s, db, "FLUSHDB").await;
    let big_text: String = "héllo wörld ".repeat(1_747_627);
    let big_bytes: Vec<u8> = (0..20 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    put(
        &*s,
        db,
        vec![
            KeyChange::Put {
                key: "big:text".into(),
                data: KeyData::String {
                    value: Value::Text(big_text.clone()),
                },
                ttl_ms: None,
                mode: PutMode::Replace,
            },
            KeyChange::Put {
                key: "big:bytes".into(),
                data: KeyData::String {
                    value: Value::Bytes(big_bytes.clone()),
                },
                ttl_ms: None,
                mode: PutMode::Replace,
            },
        ],
    )
    .await;
    let read = |name: &'static str| {
        let s = s.clone();
        async move {
            let p = s
                .read_key(KeyReadRequest {
                    key: db_path(db).child(ObjectKind::Key, name),
                    cursor: None,
                    count: 10,
                    pattern: None,
                    max_string_bytes: Some(64 * 1024 * 1024),
                })
                .await
                .unwrap();
            match p.entries {
                dbstudio_core::KeyEntries::String {
                    value, truncated, ..
                } => {
                    assert!(!truncated);
                    digest(&value)
                }
                other => panic!("{other:?}"),
            }
        }
    };
    let want = (read("big:text").await, read("big:bytes").await);
    // 1_747_627 x "héllo wörld " = 24,466,778 bytes of UTF-8.
    assert!(want.0.starts_with("t24466778:"), "{want:?}");
    assert!(want.1.starts_with("b20971520:"), "{want:?}");
    let dir = tempfile::tempdir().unwrap();
    let report = run_export(
        redis_ctx(s.clone()),
        ExportSpec {
            connection_id: Uuid::nil(),
            source: ExportSource::Keys {
                database: db_path(db),
                pattern: "big:*".into(),
            },
            content: ExportContent::Data,
            format: ExportFormat::Json(JsonOptions {
                layout: JsonLayout::Ndjson,
                ..JsonOptions::default()
            }),
            destination: ExportDestination {
                path: dir.path().join("big.ndjson").to_string_lossy().into_owned(),
                overwrite: false,
            },
            values: ValueFormat::default(),
        },
    )
    .await
    .unwrap();
    assert_eq!(report.rows, 2, "{:?}", report.warnings);
    redis_cmd(&*s, db, "FLUSHDB").await;
    let imported = run_import(
        redis_ctx(s.clone()),
        ImportSpec {
            connection_id: Uuid::nil(),
            file: report.files[0].clone(),
            format: ImportFormat::Json {
                layout: JsonLayout::Ndjson,
            },
            target: ImportTarget::Keys {
                database: db_path(db),
                mode: PutMode::Replace,
            },
            mapping: vec![],
            on_conflict: None,
            mode: ImportMode::Batches { rows: 10 },
            max_errors: 0,
        },
    )
    .await
    .unwrap();
    assert_eq!(imported.inserted, 2, "{imported:?}");
    assert_eq!((read("big:text").await, read("big:bytes").await), want);
    redis_cmd(&*s, db, "FLUSHDB").await;
}

/// Large strings and binary values (7 MB each: a BSON document stops at 16 MB) survive the JSON
/// (EJSON) export and import.
#[tokio::test(flavor = "multi_thread")]
async fn mongo_large_values_round_trip() {
    if !enabled() {
        return;
    }
    let Some(s) = mongo_connect(true).await else {
        return;
    };
    let db = format!("dbs_lob_{}", Uuid::new_v4().simple());
    let _guard = DropDatabase {
        replica: true,
        db: db.clone(),
    };
    let dir = tempfile::tempdir().unwrap();
    // Seed through the import itself: one NDJSON record with both values.
    use base64::Engine as _;
    let bytes: Vec<u8> = (0..7 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let text = "héllo wörld ".repeat(7_000_000 / 13);
    let seed = dir.path().join("seed.ndjson");
    let line = format!(
        "{{\"_id\":1,\"t\":{},\"b\":{{\"$binary\":{{\"base64\":\"{}\",\"subType\":\"00\"}}}}}}\n",
        serde_json::to_string(&text).unwrap(),
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    );
    std::fs::write(&seed, line).unwrap();
    let target = |name: &str| ImportTarget::New {
        parent: ObjectPath::root().child(ObjectKind::Database, &db),
        name: name.into(),
        columns: vec![],
    };
    let ndjson_in = ImportFormat::Json {
        layout: JsonLayout::Ndjson,
    };
    run_import(
        mongo_ctx(s.clone()),
        import_spec(&seed, ndjson_in.clone(), target("src")),
    )
    .await
    .unwrap_or_else(|e| panic!("seed: {e}"));
    let fingerprint = |coll: &'static str| {
        let (s, db) = (s.clone(), db.clone());
        async move {
            let mut sink = CollectingSink::new();
            s.fetch_table(
                TableDataRequest {
                    query_id: Uuid::new_v4(),
                    table: coll_path(&db, coll),
                    filters: vec![],
                    raw_where: None,
                    sort: vec![],
                    offset: 0,
                    limit: 10,
                    batch_size: Some(1),
                },
                &mut sink,
            )
            .await
            .unwrap();
            match sink.into_results().remove(0).outcome {
                StatementOutcome::Rows(rs) => digest(&rs.rows[0][0]),
                other => panic!("{other:?}"),
            }
        }
    };
    let want = fingerprint("src").await;
    assert!(want.contains("b7340032:"), "{want}");
    let out = dir.path().join("out.ndjson");
    let report = run_export(
        mongo_ctx(s.clone()),
        export_spec(
            &db,
            "src",
            ExportFormat::Json(JsonOptions {
                layout: JsonLayout::Ndjson,
                extended_json: ExtendedJson::Relaxed,
                ..JsonOptions::default()
            }),
            ValueFormat::default(),
            &out,
        ),
    )
    .await
    .unwrap();
    run_import(
        mongo_ctx(s.clone()),
        import_spec(Path::new(&report.files[0]), ndjson_in, target("copy")),
    )
    .await
    .unwrap_or_else(|e| panic!("copy: {e}"));
    assert_eq!(fingerprint("copy").await, want);
}
