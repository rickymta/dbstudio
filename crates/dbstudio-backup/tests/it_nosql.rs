//! Integration tests (`DBSTUDIO_IT=1`): logical backup -> restore for MongoDB 7 / 4.4 and
//! Redis 7 (TLS) / Valkey 8. A server that is not running is skipped unless
//! `DBSTUDIO_IT_STRICT=1`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dbstudio_backup::{
    BackupContext, BackupEngine, BackupSpec, Compression, Destination, RestoreSpec, inspect,
    run_backup, run_restore, verify,
};
use dbstudio_core::{
    CancelToken, ConnectionConfig, DbKind, DdlRequest, Driver, JobProgress, JobReporter,
    ObjectKind, ObjectPath, QueryRequest, Secret, Session, StatementOutcome, TlsMode, Value,
    execute_collect,
};
use dbstudio_export::{
    ExportContent, ExportContext, ExportDestination, ExportFormat, ExportSource, ExportSpec,
    ExtendedJson, JsonLayout, JsonOptions, ValueFormat, run_export,
};
use uuid::Uuid;

fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").as_deref() == Ok("1")
}

fn strict() -> bool {
    std::env::var("DBSTUDIO_IT_STRICT").as_deref() == Ok("1")
}

fn port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[derive(Default)]
struct Rep {
    cancel: CancelToken,
    logs: Mutex<Vec<String>>,
}

impl JobReporter for Rep {
    fn progress(&self, _: &JobProgress) {}
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

fn ctx(cfg: &ConnectionConfig, s: &Arc<dyn Session>, rep: &Arc<Rep>) -> BackupContext {
    BackupContext {
        session: s.clone(),
        config: cfg.clone(),
        secret: Some(Secret::new("dbstudio_test")),
        tunnel_addr: None,
        reporter: rep.clone(),
        job_name: Some("it".into()),
        tool_overrides: BTreeMap::new(),
        work_dir: None,
    }
}

fn spec(target: ObjectPath, dir: &std::path::Path, encrypted: bool) -> BackupSpec {
    BackupSpec {
        connection_id: Uuid::nil(),
        targets: vec![target],
        engine: BackupEngine::Logical {
            content: ExportContent::Both,
            ddl: Default::default(),
        },
        compression: Compression::Zstd { level: 3 },
        encrypted,
        destination: Destination::LocalDir {
            path: dir.to_string_lossy().into_owned(),
            file_template: "{database}-{date:%H%M%S}.dbsbak".into(),
        },
    }
}

fn restore_spec(
    file: &str,
    target: Option<&str>,
    clean: bool,
    objects: Vec<String>,
) -> RestoreSpec {
    RestoreSpec {
        file: file.into(),
        target_connection_id: Uuid::nil(),
        target_database: target.map(str::to_string),
        create_database: false,
        clean,
        objects,
        continue_on_error: false,
    }
}

async fn run(s: &dyn Session, db: Option<&str>, text: &str) -> Vec<StatementOutcome> {
    let mut req = QueryRequest::new(text);
    req.database = db.map(str::to_string);
    execute_collect(s, req)
        .await
        .unwrap_or_else(|e| panic!("{text}: {e}"))
        .into_iter()
        .map(|r| r.outcome)
        .collect()
}

fn first_int(o: &[StatementOutcome]) -> i64 {
    match &o[0] {
        StatementOutcome::Rows(r) => match &r.rows[0][0] {
            Value::Int(i) => *i,
            Value::UInt(u) => *u as i64,
            Value::Document(d) => match d.first().map(|f| &f.1) {
                Some(Value::Int(i)) => *i,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// MongoDB
// ---------------------------------------------------------------------------------------------

async fn mongo_connect(replica: bool) -> Option<(ConnectionConfig, Arc<dyn Session>)> {
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
        Ok(s) => Some((cfg, Arc::from(s))),
        Err(e) if strict() => panic!("{name} is not reachable: {e}"),
        Err(e) => {
            eprintln!("skipping {name}: {e}");
            None
        }
    }
}

fn db_node(db: &str) -> ObjectPath {
    ObjectPath::root().child(ObjectKind::Database, db)
}

fn coll_node(db: &str, c: &str) -> ObjectPath {
    db_node(db)
        .child(ObjectKind::Folder, "collections")
        .child(ObjectKind::Collection, c)
}

/// Canonical Extended JSON of every document, sorted (compares BSON types exactly).
async fn ejson_lines(s: &Arc<dyn Session>, db: &str, c: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("x.ndjson");
    run_export(
        ExportContext {
            session: s.clone(),
            kind: DbKind::MongoDb,
            dialect: dbstudio_core::ScriptDialect::MongoShell,
            reporter: Arc::new(Rep::default()),
        },
        ExportSpec {
            connection_id: Uuid::nil(),
            source: ExportSource::Tables {
                paths: vec![coll_node(db, c)],
            },
            content: ExportContent::Data,
            format: ExportFormat::Json(JsonOptions {
                layout: JsonLayout::Ndjson,
                extended_json: ExtendedJson::Canonical,
                pretty: false,
                fields: vec![],
            }),
            destination: ExportDestination {
                path: file.to_string_lossy().into_owned(),
                overwrite: true,
            },
            values: ValueFormat::default(),
        },
    )
    .await
    .unwrap();
    let mut lines: Vec<String> = std::fs::read_to_string(&file)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    lines
}

/// The statements that would re-create `c`, with the database name masked.
async fn ddl_of(s: &Arc<dyn Session>, db: &str, c: &str, view: bool) -> String {
    let path = if view {
        db_node(db)
            .child(ObjectKind::Folder, "views")
            .child(ObjectKind::View, c)
    } else {
        coll_node(db, c)
    };
    s.generate_ddl(DdlRequest {
        scope: db_node(db),
        objects: vec![path],
        options: Default::default(),
    })
    .await
    .unwrap()
    .into_iter()
    .map(|i| i.sql)
    .collect::<Vec<_>>()
    .join("\n")
}

async fn collection_names(s: &Arc<dyn Session>, db: &str) -> Vec<String> {
    let mut out = Vec::new();
    for f in s.list_children(&db_node(db)).await.unwrap() {
        for c in s.list_children(&f.path).await.unwrap() {
            out.push(c.name);
        }
    }
    out.sort();
    out
}

/// Insert Extended JSON documents through the import engine (exact BSON types).
async fn seed(s: &Arc<dyn Session>, db: &str, coll: &str, docs: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("seed.ndjson");
    std::fs::write(&file, docs.join("\n")).unwrap();
    dbstudio_export::run_import(
        ExportContext {
            session: s.clone(),
            kind: DbKind::MongoDb,
            dialect: dbstudio_core::ScriptDialect::MongoShell,
            reporter: Arc::new(Rep::default()),
        },
        dbstudio_export::ImportSpec {
            connection_id: Uuid::nil(),
            file: file.to_string_lossy().into_owned(),
            format: dbstudio_export::ImportFormat::Json {
                layout: JsonLayout::Ndjson,
            },
            target: dbstudio_export::ImportTarget::Existing {
                table: coll_node(db, coll),
            },
            mapping: vec![],
            on_conflict: None,
            mode: dbstudio_export::ImportMode::Batches { rows: 10 },
            max_errors: 0,
        },
    )
    .await
    .unwrap();
}

async fn mongo_round_trip(replica: bool) {
    let Some((cfg, s)) = mongo_connect(replica).await else {
        return;
    };
    let src = format!("bk_src_{}", unique());
    let dst = format!("bk_dst_{}", unique());
    run(
        &*s,
        Some(&src),
        r#"db.createCollection("people", {"validator": {"$jsonSchema": {"bsonType": "object", "required": ["name"]}}, "validationLevel": "moderate"})"#,
    )
    .await;
    run(&*s, Some(&src), r#"db.createCollection("logs")"#).await;
    run(&*s, Some(&src), r#"db.createCollection("empty")"#).await;
    seed(
        &s,
        &src,
        "people",
        &[
            r#"{"_id": {"$oid": "65a1b2c3d4e5f60718293a4b"}, "name": "An", "born": {"$date": "2001-02-03T04:05:06.789Z"}, "big": {"$numberLong": "9007199254740993"}, "money": {"$numberDecimal": "1234567.8900"}, "blob": {"$binary": {"base64": "AQID/w==", "subType": "00"}}, "tags": ["a", "b"], "nested": {"x": {"$numberInt": "7"}, "y": null}, "ratio": {"$numberDouble": "0.1"}, "when": {"$timestamp": {"t": 1700000000, "i": 3}}}"#,
            r#"{"_id": "text-id", "name": "Bình", "born": {"$date": "1969-07-20T20:17:00Z"}}"#,
            r#"{"_id": {"$numberInt": "42"}, "name": "Chi", "uuid": {"$uuid": "123e4567-e89b-12d3-a456-426614174000"}}"#,
        ],
    )
    .await;
    run(
        &*s,
        Some(&src),
        r#"db.logs.insertMany([{"n": 1}, {"n": 2}, {"n": 3}])"#,
    )
    .await;
    run(
        &*s,
        Some(&src),
        r#"db.people.createIndex({"name": 1}, {"unique": true, "name": "by_name"})"#,
    )
    .await;
    run(
        &*s,
        Some(&src),
        r#"db.logs.createIndex({"n": -1}, {"name": "n_desc", "expireAfterSeconds": 3600})"#,
    )
    .await;
    run(
        &*s,
        Some(&src),
        r#"db.createView("big_logs", "logs", [{"$match": {"n": {"$gte": 2}}}])"#,
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let rep = Arc::new(Rep::default());
    let pass = Secret::new("pw");
    let out = run_backup(
        ctx(&cfg, &s, &rep),
        &spec(db_node(&src), dir.path(), true),
        Some(&pass),
    )
    .await
    .unwrap();
    let file = PathBuf::from(&out.files[0]);
    assert_eq!(out.rows, 6);
    let ins = inspect(&file, Some(&pass)).await.unwrap();
    let pre = ins.preamble.unwrap();
    assert_eq!(pre["engine"], "logical-mongo");
    assert_eq!(pre["formatVersion"], 3);
    assert_eq!(ins.objects.len(), 4, "{pre}");
    verify(&file, Some(&pass), &*rep).await.unwrap();

    // Whole restore into an empty database.
    let res = run_restore(
        ctx(&cfg, &s, &rep),
        &restore_spec(file.to_str().unwrap(), Some(&dst), false, vec![]),
        Some(&pass),
    )
    .await
    .unwrap();
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    assert_eq!(res.rows, 6);
    assert_eq!(
        collection_names(&s, &dst).await,
        ["big_logs", "empty", "logs", "people"]
    );
    for c in ["people", "logs", "empty"] {
        assert_eq!(
            ejson_lines(&s, &src, c).await,
            ejson_lines(&s, &dst, c).await,
            "documents of {c}"
        );
    }
    for (c, view) in [("people", false), ("logs", false), ("big_logs", true)] {
        let a = ddl_of(&s, &src, c, view).await.replace(&src, "<DB>");
        let b = ddl_of(&s, &dst, c, view).await.replace(&dst, "<DB>");
        assert_eq!(a, b, "structure of {c}");
    }
    assert!(
        ddl_of(&s, &dst, "people", false).await.contains("$jsonSchema"),
        "validator kept"
    );
    assert_eq!(
        first_int(&run(&*s, Some(&dst), r#"db.big_logs.countDocuments({})"#).await),
        2
    );
    // The unique index is enforced again.
    let dup = execute_collect(
        &*s,
        {
            let mut r = QueryRequest::new(r#"db.people.insertOne({"name": "An"})"#);
            r.database = Some(dst.clone());
            r
        },
    )
    .await;
    assert!(dup.is_err(), "unique index must exist after restore");

    // Partial restore: one collection (by name) into a fresh database.
    let part = format!("bk_part_{}", unique());
    let res = run_restore(
        ctx(&cfg, &s, &rep),
        &restore_spec(file.to_str().unwrap(), Some(&part), false, vec!["logs".into()]),
        Some(&pass),
    )
    .await
    .unwrap();
    assert_eq!((res.objects, res.rows), (1, 3));
    assert_eq!(collection_names(&s, &part).await, ["logs"]);
    // An unknown name is refused before anything is written.
    let bad = run_restore(
        ctx(&cfg, &s, &rep),
        &restore_spec(file.to_str().unwrap(), Some(&part), false, vec!["nope".into()]),
        Some(&pass),
    )
    .await;
    assert!(bad.is_err());

    // `clean` replaces the collections: the extra document disappears.
    run(&*s, Some(&dst), r#"db.logs.insertOne({"n": 99})"#).await;
    let res = run_restore(
        ctx(&cfg, &s, &rep),
        &restore_spec(file.to_str().unwrap(), Some(&dst), true, vec![]),
        Some(&pass),
    )
    .await
    .unwrap();
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    assert_eq!(
        ejson_lines(&s, &src, "logs").await,
        ejson_lines(&s, &dst, "logs").await
    );

    for db in [&src, &dst, &part] {
        run(&*s, Some(db), "db.dropDatabase()").await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mongo7_backup_restore_round_trip() {
    if enabled() {
        mongo_round_trip(true).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mongo44_backup_restore_round_trip() {
    if enabled() {
        mongo_round_trip(false).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Redis / Valkey
// ---------------------------------------------------------------------------------------------

async fn redis_connect(
    name: &'static str,
    var: &str,
    default_port: u16,
    tls: bool,
) -> Option<(ConnectionConfig, Arc<dyn Session>)> {
    let mut cfg = ConnectionConfig::new(DbKind::Redis, name, "127.0.0.1");
    cfg.port = port(var, default_port);
    cfg.username = Some("dbstudio".into());
    cfg.tls.mode = if tls { TlsMode::Require } else { TlsMode::Disable };
    match dbstudio_driver_redis::RedisDriver::new()
        .connect(&cfg, Some(&Secret::new("dbstudio_test")))
        .await
    {
        Ok(s) => Some((cfg, Arc::from(s))),
        Err(e) if strict() => panic!("{name} is not reachable: {e}"),
        Err(e) => {
            eprintln!("skipping {name}: {e}");
            None
        }
    }
}

/// Every key of database `db` as JSON, without the absolute expiry, sorted.
async fn key_dump(s: &Arc<dyn Session>, db: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("k.ndjson");
    run_export(
        ExportContext {
            session: s.clone(),
            kind: DbKind::Redis,
            dialect: dbstudio_core::ScriptDialect::RedisCli,
            reporter: Arc::new(Rep::default()),
        },
        ExportSpec {
            connection_id: Uuid::nil(),
            source: ExportSource::Keys {
                database: db_node(db),
                pattern: String::new(),
            },
            content: ExportContent::Data,
            format: ExportFormat::Json(JsonOptions {
                layout: JsonLayout::Ndjson,
                extended_json: ExtendedJson::Off,
                pretty: false,
                fields: vec![],
            }),
            destination: ExportDestination {
                path: file.to_string_lossy().into_owned(),
                overwrite: true,
            },
            values: ValueFormat::default(),
        },
    )
    .await
    .unwrap();
    let mut out: Vec<String> = std::fs::read_to_string(&file)
        .unwrap_or_default()
        .lines()
        .map(|l| {
            let mut v: serde_json::Value = serde_json::from_str(l).unwrap();
            v.as_object_mut().unwrap().remove("expireAtMs");
            v.to_string()
        })
        .collect();
    out.sort();
    out
}

async fn redis_round_trip(
    name: &'static str,
    var: &str,
    default_port: u16,
    tls: bool,
    db: &str,
) {
    let Some((cfg, s)) = redis_connect(name, var, default_port, tls).await else {
        return;
    };
    run(&*s, Some(db), "FLUSHDB").await;
    for cmd in [
        "SET str:plain hello",
        "SET str:ttl world EX 3600",
        "HSET h a 1 b 2",
        "RPUSH l x y z",
        "SADD st 1 2 3",
        "ZADD z 1.5 one 2.5 two",
    ] {
        run(&*s, Some(db), cmd).await;
    }
    let before = key_dump(&s, db).await;
    assert_eq!(before.len(), 6);

    let dir = tempfile::tempdir().unwrap();
    let rep = Arc::new(Rep::default());
    let out = run_backup(
        ctx(&cfg, &s, &rep),
        &spec(db_node(db), dir.path(), false),
        None,
    )
    .await
    .unwrap();
    assert_eq!(out.rows, 6);
    let file = PathBuf::from(&out.files[0]);
    let ins = inspect(&file, None).await.unwrap();
    assert_eq!(ins.preamble.unwrap()["engine"], "logical-redis");
    verify(&file, None, &*rep).await.unwrap();
    assert!(
        rep.logs
            .lock()
            .unwrap()
            .iter()
            .all(|l| !l.contains("snapshot")),
        "the snapshot note is in the preamble"
    );

    run(&*s, Some(db), "FLUSHDB").await;
    assert_eq!(first_int(&run(&*s, Some(db), "DBSIZE").await), 0);
    let res = run_restore(
        ctx(&cfg, &s, &rep),
        &restore_spec(file.to_str().unwrap(), Some(db), false, vec![]),
        None,
    )
    .await
    .unwrap();
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    assert_eq!(res.rows, 6);
    assert_eq!(key_dump(&s, db).await, before);
    let ttl = first_int(&run(&*s, Some(db), "PTTL str:ttl").await);
    assert!(ttl > 3_000_000 && ttl <= 3_600_000, "ttl {ttl}");
    assert_eq!(first_int(&run(&*s, Some(db), "PTTL str:plain").await), -1);

    // Without `clean` a key that exists is kept; with `clean` the database is replaced.
    run(&*s, Some(db), "SET str:plain changed").await;
    run(&*s, Some(db), "SET extra 1").await;
    run_restore(
        ctx(&cfg, &s, &rep),
        &restore_spec(file.to_str().unwrap(), Some(db), false, vec![]),
        None,
    )
    .await
    .unwrap();
    assert_eq!(first_int(&run(&*s, Some(db), "DBSIZE").await), 7);
    run_restore(
        ctx(&cfg, &s, &rep),
        &restore_spec(file.to_str().unwrap(), Some(db), true, vec![]),
        None,
    )
    .await
    .unwrap();
    assert_eq!(key_dump(&s, db).await, before);
    run(&*s, Some(db), "FLUSHDB").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn redis7_backup_restore_round_trip() {
    if enabled() {
        redis_round_trip("redis7", "DBSTUDIO_IT_REDIS7_PORT", 63797, true, "12").await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn valkey8_backup_restore_round_trip() {
    if enabled() {
        redis_round_trip("valkey8", "DBSTUDIO_IT_VALKEY8_PORT", 63798, false, "12").await;
    }
}
