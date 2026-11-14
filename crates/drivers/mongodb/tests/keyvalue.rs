//! Package K integration tests: concurrent document edits (`expected`), `$document` bulk
//! inserts, TLS name through a SOCKS5 proxy over SSH. Against `mongo44` (standalone) and `mongo7`
//! (replica set rs0). They run only with `DBSTUDIO_IT=1`.

#![allow(dead_code, unused_imports)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use dbstudio_core::*;
use dbstudio_driver_mongodb::MongoDriver;

fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").is_ok_and(|v| v == "1")
}

fn port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Srv {
    M44,
    M7,
}

fn config(srv: Srv) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new(DbKind::MongoDb, "it", "127.0.0.1");
    cfg.port = match srv {
        Srv::M44 => port("DBSTUDIO_IT_MONGO44_PORT", 27044),
        Srv::M7 => port("DBSTUDIO_IT_MONGO7_PORT", 27077),
    };
    cfg.username = Some("dbstudio".into());
    // Plain server: skip the TLS probe so tests stay fast (`prefer` is covered separately).
    cfg.tls.mode = if srv == Srv::M44 {
        TlsMode::Disable
    } else {
        TlsMode::Prefer
    };
    cfg.options = BTreeMap::from([
        ("authSource".to_string(), "admin".to_string()),
        ("directConnection".to_string(), "true".to_string()),
    ]);
    cfg
}

async fn connect_cfg(cfg: &ConnectionConfig) -> Result<Box<dyn Session>> {
    MongoDriver::new()
        .connect(cfg, Some(&Secret::new("dbstudio_test")))
        .await
}

async fn connect(srv: Srv) -> Box<dyn Session> {
    connect_cfg(&config(srv)).await.expect("connect")
}

fn unique_db() -> String {
    format!("dbs_it_{}", uuid::Uuid::new_v4().simple())
}

async fn run(s: &dyn Session, db: &str, text: &str) -> Vec<StatementResult> {
    let mut req = QueryRequest::new(text);
    req.database = Some(db.into());
    execute_collect(s, req)
        .await
        .unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn docs(r: &StatementResult) -> Vec<Value> {
    match &r.outcome {
        StatementOutcome::Rows(rs) => rs.rows.iter().map(|row| row[0].clone()).collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn coll_path(db: &str, coll: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Database, db)
        .child(ObjectKind::Folder, "collections")
        .child(ObjectKind::Collection, coll)
}

async fn fetch(
    s: &dyn Session,
    db: &str,
    coll: &str,
    req: impl FnOnce(&mut TableDataRequest),
) -> Vec<Value> {
    let mut r = TableDataRequest {
        query_id: uuid::Uuid::new_v4(),
        table: coll_path(db, coll),
        filters: vec![],
        raw_where: None,
        sort: vec![],
        offset: 0,
        limit: 1000,
        batch_size: Some(2),
    };
    req(&mut r);
    let mut sink = CollectingSink::new();
    s.fetch_table(r, &mut sink).await.expect("fetch_table");
    let res = sink.into_results();
    docs(&res[0])
}

async fn drop_db(s: &dyn Session, db: &str) {
    let _ = run(s, db, "db.dropDatabase()").await;
}

fn ext(v: &Value, key: &str) -> Value {
    v.doc_get(key)
        .cloned()
        .unwrap_or_else(|| panic!("missing {key} in {v:?}"))
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn set(db: &str, changes: Vec<DocumentChange>) -> ChangeSet {
    ChangeSet::Documents(DocumentChangeSet {
        collection: coll_path(db, "c"),
        changes,
    })
}

fn orig(path: &[&str], value: Option<Value>) -> FieldOriginal {
    FieldOriginal {
        path: path.iter().map(|s| s.to_string()).collect(),
        value,
    }
}

fn set_field(path: &[&str], value: Value) -> FieldEdit {
    FieldEdit {
        path: path.iter().map(|s| s.to_string()).collect(),
        value,
    }
}

fn changed_message(e: Error) -> String {
    match e {
        Error::ChangeFailed { message, .. } => message,
        other => panic!("{other:?}"),
    }
}

async fn concurrent_document_edits_are_detected(srv: Srv) {
    let s = connect(srv).await;
    let other = connect(srv).await;
    let db = unique_db();
    run(
        &*s,
        &db,
        "db.c.insertMany([{_id: 1, a: 1, n: {b: 2}, name: 'x'}, {_id: 2, a: 1}, {_id: 3, a: 1}, {_id: 4, a: 1}])",
    )
    .await;
    // Unchanged originals apply (also nested paths and "was absent").
    let r = s
        .apply_changes(set(
            &db,
            vec![DocumentChange::Update {
                id: Value::Int(1),
                set: vec![set_field(&["n", "b"], Value::Int(3))],
                unset: vec![],
                expected: vec![orig(&["n", "b"], Some(Value::Int(2))), orig(&["zz"], None)],
            }],
        ))
        .await
        .unwrap();
    assert_eq!(r.affected, vec![1]);

    // Another client changes `a` after this client read it: Update, Replace and Delete fail.
    run(&*other, &db, "db.c.updateOne({_id: 1}, {$set: {a: 99}})").await;
    let msg = changed_message(
        s.apply_changes(set(
            &db,
            vec![DocumentChange::Update {
                id: Value::Int(1),
                set: vec![set_field(&["name"], text("mine"))],
                unset: vec![],
                expected: vec![orig(&["a"], Some(Value::Int(1)))],
            }],
        ))
        .await
        .unwrap_err(),
    );
    assert_eq!(msg, "document was modified by another client");
    let msg = changed_message(
        s.apply_changes(set(
            &db,
            vec![DocumentChange::Replace {
                id: Value::Int(1),
                document: DocInput::Ejson {
                    text: r#"{"a": 5}"#.into(),
                },
                expected: vec![
                    orig(&["a"], Some(Value::Int(1))),
                    orig(&["name"], Some(text("x"))),
                ],
            }],
        ))
        .await
        .unwrap_err(),
    );
    assert_eq!(msg, "document was modified by another client");
    let msg = changed_message(
        s.apply_changes(set(
            &db,
            vec![DocumentChange::Delete {
                id: Value::Int(1),
                expected: vec![orig(&["a"], Some(Value::Int(1)))],
            }],
        ))
        .await
        .unwrap_err(),
    );
    assert_eq!(msg, "document was modified by another client");
    // The other client's data is intact.
    let rows = fetch(&*s, &db, "c", |r| {
        r.filters = vec![ColumnFilter {
            column: "_id".into(),
            op: FilterOp::Eq,
            values: vec!["1".into()],
        }]
    })
    .await;
    assert_eq!(ext(&rows[0], "a"), Value::Int(99));
    assert_eq!(ext(&rows[0], "name"), text("x"));

    // A field that was absent but exists now.
    run(&*other, &db, "db.c.updateOne({_id: 2}, {$set: {zz: 1}})").await;
    let msg = changed_message(
        s.apply_changes(set(
            &db,
            vec![DocumentChange::Update {
                id: Value::Int(2),
                set: vec![set_field(&["a"], Value::Int(2))],
                unset: vec![],
                expected: vec![orig(&["zz"], None)],
            }],
        ))
        .await
        .unwrap_err(),
    );
    assert_eq!(msg, "document was modified by another client");

    // The document is gone.
    run(&*other, &db, "db.c.deleteOne({_id: 3})").await;
    let msg = changed_message(
        s.apply_changes(set(
            &db,
            vec![DocumentChange::Delete {
                id: Value::Int(3),
                expected: vec![orig(&["a"], Some(Value::Int(1)))],
            }],
        ))
        .await
        .unwrap_err(),
    );
    assert_eq!(msg, "document no longer exists");

    // With the real values everything applies, Delete included.
    let r = s
        .apply_changes(set(
            &db,
            vec![
                DocumentChange::Delete {
                    id: Value::Int(4),
                    expected: vec![orig(&["a"], Some(Value::Int(1)))],
                },
                DocumentChange::Replace {
                    id: Value::Int(1),
                    document: DocInput::Ejson {
                        text: r#"{"a": 100}"#.into(),
                    },
                    expected: vec![orig(&["a"], Some(Value::Int(99)))],
                },
            ],
        ))
        .await
        .unwrap();
    assert_eq!(r.affected, vec![1, 1]);

    // A conflict later in the batch: replica set rolls back everything, standalone stops.
    let err = s
        .apply_changes(set(
            &db,
            vec![
                DocumentChange::Update {
                    id: Value::Int(1),
                    set: vec![set_field(&["a"], Value::Int(101))],
                    unset: vec![],
                    expected: vec![orig(&["a"], Some(Value::Int(100)))],
                },
                DocumentChange::Update {
                    id: Value::Int(2),
                    set: vec![set_field(&["a"], Value::Int(7))],
                    unset: vec![],
                    expected: vec![orig(&["a"], Some(Value::Int(1000)))],
                },
            ],
        ))
        .await
        .unwrap_err();
    let Error::ChangeFailed {
        index,
        applied,
        message,
    } = err
    else {
        panic!()
    };
    assert_eq!(index, 1);
    assert_eq!(message, "document was modified by another client");
    let one = fetch(&*s, &db, "c", |r| {
        r.filters = vec![ColumnFilter {
            column: "_id".into(),
            op: FilterOp::Eq,
            values: vec!["1".into()],
        }]
    })
    .await;
    if srv == Srv::M7 {
        assert_eq!(applied, 0);
        assert_eq!(ext(&one[0], "a"), Value::Int(100), "rolled back");
    } else {
        assert_eq!(applied, 1);
        assert_eq!(ext(&one[0], "a"), Value::Int(101));
    }
    // Preview shows the guard conditions.
    let p = s
        .preview_changes(&set(
            &db,
            vec![DocumentChange::Delete {
                id: Value::Int(1),
                expected: vec![orig(&["a"], Some(Value::Int(100)))],
            }],
        ))
        .await
        .unwrap();
    assert!(
        p[0].contains("{\"_id\": 1, \"a\": {\"$eq\": 100}}"),
        "{p:?}"
    );
    drop_db(&*s, &db).await;
}

#[tokio::test]
async fn concurrent_edits_mongo44() {
    if enabled() {
        concurrent_document_edits_are_detected(Srv::M44).await;
    }
}

#[tokio::test]
async fn concurrent_edits_mongo7() {
    if enabled() {
        concurrent_document_edits_are_detected(Srv::M7).await;
    }
}

fn bulk(db: &str, rows: Vec<Value>, on_conflict: OnConflict) -> BulkInsertRequest {
    BulkInsertRequest {
        table: coll_path(db, "c"),
        columns: vec!["$document".into()],
        rows: rows
            .into_iter()
            .map(|v| vec![EditValue::Value(v)])
            .collect(),
        on_conflict,
        conflict_columns: vec![],
    }
}

async fn document_rows_insert_whole_documents(srv: Srv) {
    let s = connect(srv).await;
    let db = unique_db();
    let lease = s
        .open_lease(LeaseOptions {
            database: None,
            schema: None,
            auto_commit: true,
        })
        .await
        .unwrap();
    // Canonical EJSON text and a ready document value; BSON types survive.
    let canonical = Value::Json(
        r#"{"_id": {"$oid": "65f0a1b2c3d4e5f607182930"},
            "price": {"$numberDecimal": "19.99"},
            "at": {"$date": {"$numberLong": "1700000000123"}},
            "big": {"$numberLong": "9007199254740993"},
            "nested": {"list": [1, {"$numberDouble": "2.5"}, null]}}"#
            .into(),
    );
    let relaxed =
        Value::Text(r#"{"_id": 2, "when": {"$date": "2024-01-02T03:04:05.678Z"}}"#.into());
    let document = Value::Document(vec![
        ("_id".into(), Value::Int(3)),
        ("s".into(), text("v")),
        (
            "o".into(),
            Value::Document(vec![("k".into(), Value::Bool(true))]),
        ),
    ]);
    let r = lease
        .bulk_insert(bulk(
            &db,
            vec![canonical, relaxed, document],
            OnConflict::Error,
        ))
        .await
        .unwrap();
    assert_eq!((r.inserted, r.skipped, r.updated), (3, 0, 0));
    let rows = fetch(&*s, &db, "c", |_| {}).await;
    assert_eq!(rows.len(), 3);
    let first = rows
        .iter()
        .find(|d| matches!(ext(d, "_id"), Value::Ext(_)))
        .unwrap();
    let Value::Ext(id) = ext(first, "_id") else {
        panic!()
    };
    assert_eq!(
        (id.ty.as_str(), id.text.as_str()),
        ("objectId", "65f0a1b2c3d4e5f607182930")
    );
    assert_eq!(ext(first, "price"), Value::Decimal("19.99".into()));
    assert_eq!(
        ext(first, "at"),
        Value::Timestamp("2023-11-14T22:13:20.123Z".into())
    );
    let Value::Ext(big) = ext(first, "big") else {
        panic!("Int64 beyond 2^53 must stay exact: {first:?}")
    };
    assert_eq!(
        (big.ty.as_str(), big.text.as_str()),
        ("int64", "9007199254740993")
    );
    assert_eq!(
        ext(first, "nested").doc_get("list"),
        Some(&Value::Array(vec![
            Value::Int(1),
            Value::Float(2.5),
            Value::Null
        ]))
    );
    finish(&*s, &db, lease).await;
}

async fn finish(s: &dyn Session, db: &str, lease: Box<dyn Lease>) {
    let _ = lease.close().await;
    drop_db(s, db).await;
}

#[tokio::test]
async fn document_rows_mongo44() {
    if enabled() {
        document_rows_insert_whole_documents(Srv::M44).await;
    }
}

#[tokio::test]
async fn document_rows_mongo7() {
    if enabled() {
        document_rows_insert_whole_documents(Srv::M7).await;
    }
}

async fn document_rows_conflict_modes(srv: Srv) {
    let s = connect(srv).await;
    let db = unique_db();
    let lease = s
        .open_lease(LeaseOptions {
            database: None,
            schema: None,
            auto_commit: true,
        })
        .await
        .unwrap();
    let d = |id: i64, v: &str| {
        Value::Document(vec![("_id".into(), Value::Int(id)), ("v".into(), text(v))])
    };
    lease
        .bulk_insert(bulk(&db, vec![d(1, "a"), d(2, "b")], OnConflict::Error))
        .await
        .unwrap();
    // Error: the duplicate is reported with its row number.
    let e = lease
        .bulk_insert(bulk(
            &db,
            vec![d(10, "x"), d(2, "dup"), d(11, "y")],
            OnConflict::Error,
        ))
        .await
        .unwrap_err();
    assert!(
        e.message().contains("row 2") && e.message().contains("11000"),
        "{e}"
    );
    // Skip: duplicates are counted, the rest goes in.
    let r = lease
        .bulk_insert(bulk(
            &db,
            vec![d(2, "dup"), d(20, "z"), d(1, "dup")],
            OnConflict::Skip,
        ))
        .await
        .unwrap();
    assert_eq!((r.inserted, r.skipped), (1, 2));
    // Update: replaceOne with upsert by _id; fields missing from the new document disappear.
    run(&*s, &db, "db.c.updateOne({_id: 1}, {$set: {extra: true}})").await;
    let r = lease
        .bulk_insert(bulk(
            &db,
            vec![d(1, "replaced"), d(30, "upserted")],
            OnConflict::Update,
        ))
        .await
        .unwrap();
    assert_eq!((r.inserted, r.updated), (1, 1));
    let rows = fetch(&*s, &db, "c", |r| {
        r.filters = vec![ColumnFilter {
            column: "_id".into(),
            op: FilterOp::Eq,
            values: vec!["1".into()],
        }]
    })
    .await;
    assert_eq!(ext(&rows[0], "v"), text("replaced"));
    assert!(rows[0].doc_get("extra").is_none(), "{:?}", rows[0]);
    // A document without the conflict key cannot be matched: refused, nothing overwritten.
    let e = lease
        .bulk_insert(bulk(
            &db,
            vec![Value::Document(vec![("v".into(), text("no id"))])],
            OnConflict::Update,
        ))
        .await
        .unwrap_err();
    assert!(e.message().contains("conflict key"), "{e}");
    // Not a document / bad EJSON: error names the row.
    let e = lease
        .bulk_insert(bulk(
            &db,
            vec![d(40, "ok"), Value::Int(5)],
            OnConflict::Error,
        ))
        .await
        .unwrap_err();
    assert!(e.message().contains("row 2"), "{e}");
    let e = lease
        .bulk_insert(bulk(
            &db,
            vec![Value::Json("{bad".into())],
            OnConflict::Error,
        ))
        .await
        .unwrap_err();
    assert!(e.message().contains("row 1"), "{e}");
    finish(&*s, &db, lease).await;
}

#[tokio::test]
async fn document_conflict_modes_mongo44() {
    if enabled() {
        document_rows_conflict_modes(Srv::M44).await;
    }
}

#[tokio::test]
async fn document_conflict_modes_mongo7() {
    if enabled() {
        document_rows_conflict_modes(Srv::M7).await;
    }
}

/// SOCKS5 proxy over `sshd` to `mongo7:27017`; the first attempt reports the host key, which the
/// test then trusts. `None` (skip) when `sshd` is not running.
async fn socks_tunnel_to_mongo7() -> Option<dbstudio_ssh::Tunnel> {
    let port: u16 = std::env::var("DBSTUDIO_IT_SSH_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(22022);
    let probe = tokio::time::timeout(
        Duration::from_secs(1),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await;
    if !matches!(probe, Ok(Ok(_))) {
        assert!(
            !dbstudio_core::testing::it_strict_for("sshd"),
            "sshd is not running"
        );
        eprintln!("skip: sshd is not running on 127.0.0.1:{port}");
        return None;
    }
    let cfg = SshConfig {
        host: "127.0.0.1".into(),
        port,
        username: "dbstudio".into(),
        auth: SshAuth::Password,
        keepalive_secs: Some(15),
        connect_timeout_ms: Some(15_000),
    };
    let secret = Secret::new("dbstudio_ssh");
    let known = dbstudio_ssh::MemoryKnownHosts::new();
    let allowed = vec![("mongo7".to_string(), 27017u16)];
    let err = dbstudio_ssh::Tunnel::open_socks5(&cfg, Some(&secret), allowed.clone(), &known)
        .await
        .unwrap_err();
    let Error::SshHostKey {
        key_type,
        fingerprint,
        ..
    } = err
    else {
        panic!("{err:?}")
    };
    known.trust("127.0.0.1", port, &key_type, &fingerprint);
    Some(
        dbstudio_ssh::Tunnel::open_socks5(&cfg, Some(&secret), allowed, &known)
            .await
            .unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tls_name_is_checked_against_the_original_host_through_socks5_over_ssh() {
    if !enabled() {
        return;
    }
    let Some(tunnel) = socks_tunnel_to_mongo7().await else {
        return;
    };
    let ca = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../docker/certs/ca.crt");
    // The profile keeps the ORIGINAL host; the proxy option carries the tunnel end.
    let mut cfg = ConnectionConfig::new(DbKind::MongoDb, "it", "mongo7");
    cfg.port = 27017;
    cfg.username = Some("dbstudio".into());
    cfg.tls.mode = TlsMode::VerifyFull;
    cfg.tls.ca_cert_path = Some(ca.into());
    cfg.options = BTreeMap::from([
        ("authSource".to_string(), "admin".to_string()),
        (
            "socks5Proxy".to_string(),
            format!("127.0.0.1:{}", tunnel.local_addr().port()),
        ),
    ]);
    // Replica set discovery (no directConnection): members are announced as `mongo7:27017`,
    // which only resolves inside the Docker network, i.e. through the proxy.
    let s = connect_cfg(&cfg).await.expect("verifyFull through ssh");
    s.ping().await.unwrap();
    let info = s.server_info().await.unwrap();
    assert!(info.version.starts_with("7."), "{}", info.version);
    let db = unique_db();
    run(&*s, &db, "db.c.insertOne({_id: 1, via: 'ssh'})").await;
    assert_eq!(fetch(&*s, &db, "c", |_| {}).await.len(), 1);
    drop_db(&*s, &db).await;
    // The proxy only serves the profile's destination; another name is refused (no certificate
    // for it could ever verify anyway), and a name outside the certificate fails with `tls`.
    let mut wrong = cfg.clone();
    wrong.host = "mongo44".into();
    wrong.port = 27017;
    wrong.connect_timeout_ms = Some(3000);
    assert!(connect_cfg(&wrong).await.is_err());
}

#[tokio::test]
async fn expected_values_are_compared_as_values_and_bad_conflict_keys_are_refused() {
    if !enabled() {
        return;
    }
    let s = connect(Srv::M7).await;
    let db = unique_db();
    // A sub-document with `$` keys and a regex are plain values, not operators.
    run(
        &*s,
        &db,
        "db.c.insertOne({_id: 1, re: /ab+/i, sub: {\"$gt\": 5}, n: 1})",
    )
    .await;
    let r = s
        .apply_changes(set(
            &db,
            vec![DocumentChange::Update {
                id: Value::Int(1),
                set: vec![set_field(&["n"], Value::Int(2))],
                unset: vec![],
                expected: vec![orig(&["n"], Some(Value::Int(1)))],
            }],
        ))
        .await
        .unwrap();
    assert_eq!(r.affected, vec![1]);
    let regex_doc = fetch(&*s, &db, "c", |_| {}).await;
    let re = ext(&regex_doc[0], "re");
    let e = s
        .apply_changes(set(
            &db,
            vec![DocumentChange::Update {
                id: Value::Int(1),
                set: vec![set_field(&["n"], Value::Int(3))],
                unset: vec![],
                expected: vec![orig(&["re"], Some(re))],
            }],
        ))
        .await;
    assert!(e.is_ok(), "an unchanged regex must match itself: {e:?}");
    // `$document` Update: operator first key and dotted conflict columns are refused.
    let lease = s
        .open_lease(LeaseOptions {
            database: None,
            schema: None,
            auto_commit: true,
        })
        .await
        .unwrap();
    let mut req = bulk(
        &db,
        vec![Value::Document(vec![("$set".into(), Value::Int(1))])],
        OnConflict::Update,
    );
    let e = lease.bulk_insert(req.clone()).await.unwrap_err();
    assert!(e.message().contains("operator"), "{e}");
    req.rows = vec![vec![EditValue::Value(Value::Document(vec![(
        "_id".into(),
        Value::Int(9),
    )]))]];
    req.conflict_columns = vec!["a.b".into()];
    let e = lease.bulk_insert(req).await.unwrap_err();
    assert!(e.message().contains("not supported"), "{e}");
    finish(&*s, &db, lease).await;
}
