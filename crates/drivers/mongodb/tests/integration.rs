//! Integration tests against `mongo44` (standalone) and `mongo7` (replica set rs0, TLS preferred).
//! They run only with `DBSTUDIO_IT=1`.

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

#[tokio::test]
async fn connects_and_reports_versions() {
    if !enabled() {
        return;
    }
    for (srv, major, tx) in [(Srv::M44, "4.4", "false"), (Srv::M7, "7.", "true")] {
        let s = connect(srv).await;
        s.ping().await.unwrap();
        let info = s.server_info().await.unwrap();
        assert!(info.version.starts_with(major), "{srv:?} {}", info.version);
        assert_eq!(info.extra.get("transactions").map(String::as_str), Some(tx));
        assert_eq!(s.script_dialect(), ScriptDialect::MongoShell);
    }
}

#[tokio::test]
async fn wrong_password_is_an_authentication_error() {
    if !enabled() {
        return;
    }
    let err = MongoDriver::new()
        .connect(&config(Srv::M44), Some(&Secret::new("nope")))
        .await
        .err()
        .expect("must fail");
    assert_eq!(err.kind(), "authentication", "{err}");
}

#[tokio::test]
async fn tls_modes_against_mongo7() {
    if !enabled() {
        return;
    }
    let certs = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../docker/certs");
    for mode in [TlsMode::Prefer, TlsMode::Require, TlsMode::Disable] {
        let mut cfg = config(Srv::M7);
        cfg.tls.mode = mode;
        let s = connect_cfg(&cfg)
            .await
            .unwrap_or_else(|e| panic!("{mode:?}: {e}"));
        s.ping().await.unwrap();
    }
    // Verified TLS with the project CA (the server certificate covers localhost).
    let mut cfg = config(Srv::M7);
    cfg.host = "localhost".into();
    cfg.tls.mode = TlsMode::VerifyFull;
    cfg.tls.ca_cert_path = Some(format!("{certs}/ca.crt"));
    let s = connect_cfg(&cfg).await.expect("verifyFull with CA");
    s.ping().await.unwrap();
    // A different CA must be rejected.
    cfg.tls.ca_cert_path = Some(format!("{certs}/other-ca.crt"));
    cfg.connect_timeout_ms = Some(3000);
    let err = connect_cfg(&cfg).await.err().expect("wrong CA must fail");
    assert!(matches!(err.kind(), "tls" | "connection"), "{err}");
    // Plain server with Require: TLS handshake fails, no silent downgrade.
    let mut cfg = config(Srv::M44);
    cfg.tls.mode = TlsMode::Require;
    cfg.connect_timeout_ms = Some(3000);
    assert!(connect_cfg(&cfg).await.is_err());
}

#[tokio::test]
async fn types_round_trip_through_shell_and_value() {
    if !enabled() {
        return;
    }
    for srv in [Srv::M44, Srv::M7] {
        let s = connect(srv).await;
        let db = unique_db();
        run(
            &*s,
            &db,
            r#"db.t.insertOne({
            _id: ObjectId("65f0a1b2c3d4e5f607182930"),
            dec: NumberDecimal("12345678901234567890.123456789"),
            when: ISODate("2024-05-31T13:45:00.123Z"),
            big: NumberLong("9007199254740993"),
            small: NumberInt(7), f: 1.5, s: "héllo", b: true, n: null,
            bin: BinData(0, "AQID"), bin5: BinData(5, "AAECAwQFBgcICQoLDA0ODw=="),
            ts: Timestamp(5, 9), nested: {a: [1, {b: NumberLong(2)}]}, re: /ab+c/i
        })"#,
        )
        .await;
        let rows = fetch(&*s, &db, "t", |_| {}).await;
        assert_eq!(rows.len(), 1);
        let d = &rows[0];
        assert_eq!(
            ext(d, "_id"),
            Value::Ext(ExtValue::new("objectId", "65f0a1b2c3d4e5f607182930"))
        );
        assert_eq!(
            ext(d, "dec"),
            Value::Decimal("12345678901234567890.123456789".into())
        );
        assert_eq!(
            ext(d, "when"),
            Value::Timestamp("2024-05-31T13:45:00.123Z".into())
        );
        assert_eq!(
            ext(d, "big"),
            Value::Ext(ExtValue::new("int64", "9007199254740993"))
        );
        assert_eq!(ext(d, "small"), Value::Int(7));
        assert_eq!(ext(d, "bin"), Value::Bytes(vec![1, 2, 3]));
        assert_eq!(
            ext(d, "bin5"),
            Value::Ext(ExtValue::new("binary", "05:AAECAwQFBgcICQoLDA0ODw=="))
        );
        assert_eq!(
            ext(d, "ts"),
            Value::Ext(ExtValue::new("bsonTimestamp", "5,9"))
        );
        assert_eq!(ext(d, "re"), Value::Ext(ExtValue::new("regex", "/ab+c/i")));
        // Field order is kept.
        if let Value::Document(f) = d {
            let keys: Vec<_> = f.iter().map(|(k, _)| k.as_str()).collect();
            assert_eq!(&keys[..4], ["_id", "dec", "when", "big"]);
        }

        // Write the same Value back through apply_changes (Replace) and read it again.
        let id = ext(d, "_id");
        let changes = ChangeSet::Documents(DocumentChangeSet {
            collection: coll_path(&db, "t"),
            changes: vec![DocumentChange::Replace {
                id: id.clone(),
                document: DocInput::Value { value: d.clone() },
            }],
        });
        s.apply_changes(changes).await.unwrap();
        let again = fetch(&*s, &db, "t", |_| {}).await;
        assert_eq!(again, rows, "{srv:?}: lossless round trip");
        drop_db(&*s, &db).await;
    }
}

#[tokio::test]
async fn tree_lists_databases_collections_indexes_and_views() {
    if !enabled() {
        return;
    }
    let s = connect(Srv::M7).await;
    let db = unique_db();
    run(&*s, &db, "db.users.insertMany([{a:1},{a:2},{a:3}])").await;
    // commitQuorum 0: the test replica set cannot vote for index builds (internal TLS peer
    // certificate lacks the clientAuth purpose); real replica sets do not need it.
    run(&*s, &db, "db.users.createIndex({a: 1}, {unique: true}, 0)").await;
    run(
        &*s,
        &db,
        r#"db.createView("big_users", "users", [{$match: {a: {$gt: 1}}}])"#,
    )
    .await;
    let root = s.list_children(&ObjectPath::root()).await.unwrap();
    assert!(
        root.iter()
            .any(|n| n.name == db && n.kind == ObjectKind::Database)
    );
    let dbp = ObjectPath::root().child(ObjectKind::Database, &db);
    let kids = s.list_children(&dbp).await.unwrap();
    let names: Vec<_> = kids.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, ["collections", "views"]);
    let colls = s.list_children(&kids[0].path).await.unwrap();
    assert_eq!(colls.len(), 1);
    assert_eq!(colls[0].name, "users");
    assert_eq!(colls[0].detail.as_deref(), Some("~3 docs"));
    let views = s.list_children(&kids[1].path).await.unwrap();
    assert_eq!(views[0].kind, ObjectKind::View);
    let folder = s.list_children(&colls[0].path).await.unwrap();
    assert_eq!(folder[0].name, "indexes");
    let idx = s.list_children(&folder[0].path).await.unwrap();
    let idx_names: Vec<_> = idx.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(idx_names, ["_id_", "a_1"]);
    // Pagination fallback and search.
    let hits = s
        .search_objects(ObjectSearchRequest {
            query: "USER".into(),
            scope: dbp.clone(),
            kinds: vec![],
            limit: 10,
        })
        .await
        .unwrap();
    assert_eq!(
        hits.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
        ["users", "big_users"]
    );
    // Details: sampled columns and indexes.
    let det = s.table_details(&colls[0].path).await.unwrap();
    assert_eq!(
        det.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["_id", "a"]
    );
    assert_eq!(det.indexes.len(), 2);
    assert_eq!(det.estimated_rows, Some(3));
    drop_db(&*s, &db).await;
}

#[tokio::test]
async fn find_aggregate_count_and_browser_filters() {
    if !enabled() {
        return;
    }
    let s = connect(Srv::M44).await;
    let db = unique_db();
    run(
        &*s,
        &db,
        r#"db.p.insertMany([
        {_id: 1, name: "Ann", age: 31, addr: {city: "Hue"}, tags: ["a","b"]},
        {_id: 2, name: "Bob", age: 25, addr: {city: "Hanoi"}},
        {_id: 3, name: "Cy.d", age: 40, extra: null},
        {_id: 4, name: "Dee", age: 25}
    ])"#,
    )
    .await;
    // find with projection, sort, limit via shell chain
    let r = run(
        &*s,
        &db,
        r#"db.p.find({age: {$gte: 25}}, {name: 1, _id: 0}).sort({age: -1, name: 1}).limit(3)"#,
    )
    .await;
    let names: Vec<String> = docs(&r[0])
        .iter()
        .map(|d| match ext(d, "name") {
            Value::Text(t) => t,
            o => panic!("{o:?}"),
        })
        .collect();
    assert_eq!(names, ["Cy.d", "Ann", "Bob"]);
    // multi-stage aggregate
    let r = run(&*s, &db, r#"db.p.aggregate([{$match: {age: {$lt: 40}}}, {$group: {_id: "$age", n: {$sum: 1}}}, {$sort: {_id: 1}}])"#).await;
    let d = docs(&r[0]);
    assert_eq!(d.len(), 2);
    assert_eq!(ext(&d[0], "n"), Value::Int(2));
    // counts and distinct
    let r = run(
        &*s,
        &db,
        "db.p.countDocuments({age: 25}); db.p.estimatedDocumentCount(); db.p.distinct('age')",
    )
    .await;
    assert_eq!(ext(&docs(&r[0])[0], "count"), Value::Int(2));
    assert_eq!(ext(&docs(&r[1])[0], "count"), Value::Int(4));
    match ext(&docs(&r[2])[0], "values") {
        Value::Array(a) => assert_eq!(a.len(), 3),
        o => panic!("{o:?}"),
    }
    // max_rows truncation on a script statement
    let mut req = QueryRequest::new("db.p.find({}).sort({_id: 1})");
    req.database = Some(db.clone());
    req.max_rows = Some(3);
    req.batch_size = Some(2);
    let r = execute_collect(&*s, req).await.unwrap();
    match &r[0].outcome {
        StatementOutcome::Rows(rs) => assert!(rs.rows.len() == 3 && rs.truncated),
        o => panic!("{o:?}"),
    }
    // table browser: filter on dotted path, sort desc, offset/limit, regex escaping, null
    let f = |col: &str, op, v: &[&str]| ColumnFilter {
        column: col.into(),
        op,
        values: v.iter().map(|s| s.to_string()).collect(),
    };
    let rows = fetch(&*s, &db, "p", |r| {
        r.filters = vec![f("addr.city", FilterOp::Eq, &["Hue"])]
    })
    .await;
    assert_eq!(rows.len(), 1);
    let rows = fetch(&*s, &db, "p", |r| {
        r.filters = vec![f("name", FilterOp::Contains, &["."])]
    })
    .await;
    assert_eq!(rows.len(), 1, "regex metacharacters are literal");
    let rows = fetch(&*s, &db, "p", |r| {
        r.filters = vec![
            f("age", FilterOp::Gt, &["24"]),
            f("age", FilterOp::Lt, &["40"]),
        ]
    })
    .await;
    assert_eq!(rows.len(), 3);
    let rows = fetch(&*s, &db, "p", |r| {
        r.filters = vec![f("extra", FilterOp::IsNull, &[])]
    })
    .await;
    assert_eq!(rows.len(), 4, "null matches missing fields too");
    let rows = fetch(&*s, &db, "p", |r| {
        r.filters = vec![f("_id", FilterOp::In, &["1", "3"])]
    })
    .await;
    assert_eq!(rows.len(), 2);
    let rows = fetch(&*s, &db, "p", |r| {
        r.sort = vec![SortKey {
            column: "age".into(),
            descending: true,
        }];
        r.offset = 1;
        r.limit = 2;
    })
    .await;
    assert_eq!(
        rows.iter().map(|d| ext(d, "_id")).collect::<Vec<_>>(),
        vec![Value::Int(1), Value::Int(2)]
    );
    let rows = fetch(&*s, &db, "p", |r| {
        r.raw_where = Some(r#"{"age": {"$gt": 30}}"#.into())
    })
    .await;
    assert_eq!(rows.len(), 2);
    let rows = fetch(&*s, &db, "p", |r| {
        r.raw_where = Some("{age: {$gt: 30}, name: /^A/}".into())
    })
    .await;
    assert_eq!(rows.len(), 1, "shell syntax in raw_where");
    let bad = s
        .count_table(TableCountRequest {
            table: coll_path(&db, "p"),
            filters: vec![],
            raw_where: Some("{oops".into()),
            query_id: None,
        })
        .await;
    assert_eq!(bad.unwrap_err().kind(), "query");
    let n = s
        .count_table(TableCountRequest {
            table: coll_path(&db, "p"),
            filters: vec![],
            raw_where: None,
            query_id: None,
        })
        .await
        .unwrap();
    assert_eq!(n, 4);
    let n = s
        .count_table(TableCountRequest {
            table: coll_path(&db, "p"),
            filters: vec![f("age", FilterOp::Eq, &["25"])],
            raw_where: None,
            query_id: None,
        })
        .await
        .unwrap();
    assert_eq!(n, 2);
    // write statements report affected rows and messages
    let r = run(&*s, &db, "db.p.updateMany({age: 25}, {$set: {x: 1}}); db.p.deleteOne({_id: 4}); db.p.updateOne({_id: 99}, {$set: {n: 1}}, {upsert: true})").await;
    assert_eq!(r[0].outcome, StatementOutcome::Affected { rows: 2 });
    assert!(r[0].messages[0].contains("matched: 2"));
    assert_eq!(r[1].outcome, StatementOutcome::Affected { rows: 1 });
    assert!(r[2].messages[0].contains("upsertedId"));
    // show / use
    let r = run(&*s, &db, "show collections").await;
    assert_eq!(docs_names(&r[0]), ["p"]);
    drop_db(&*s, &db).await;
}

fn docs_names(r: &StatementResult) -> Vec<String> {
    match &r.outcome {
        StatementOutcome::Rows(rs) => rs
            .rows
            .iter()
            .map(|row| match &row[0] {
                Value::Text(t) => t.clone(),
                o => panic!("{o:?}"),
            })
            .collect(),
        o => panic!("{o:?}"),
    }
}

#[tokio::test]
async fn editing_standalone_is_sequential_and_reports_applied() {
    if !enabled() {
        return;
    }
    let s = connect(Srv::M44).await;
    let db = unique_db();
    run(
        &*s,
        &db,
        "db.e.insertMany([{_id: 1, a: 1, b: {c: 1}}, {_id: 2, a: 2}, {_id: 3, a: 3}])",
    )
    .await;
    let ed = s.editability(&coll_path(&db, "e")).await.unwrap();
    assert!(ed.editable && ed.key_columns == ["_id"] && !ed.atomic);
    let set = |changes| {
        ChangeSet::Documents(DocumentChangeSet {
            collection: coll_path(&db, "e"),
            changes,
        })
    };
    let ok = vec![
        DocumentChange::Insert {
            document: DocInput::Ejson {
                text: r#"{"_id": 4, "a": {"$numberLong": "40"}}"#.into(),
            },
        },
        DocumentChange::Update {
            id: Value::Int(1),
            set: vec![FieldEdit {
                path: vec!["b".into(), "c".into()],
                value: Value::Int(9),
            }],
            unset: vec![vec!["a".into()]],
        },
        DocumentChange::Delete { id: Value::Int(3) },
        DocumentChange::Replace {
            id: Value::Int(2),
            document: DocInput::Ejson {
                text: r#"{"z": 1}"#.into(),
            },
        },
    ];
    let preview = s.preview_changes(&set(ok.clone())).await.unwrap();
    assert!(preview[0].contains("deleteOne"), "{preview:?}");
    let res = s.apply_changes(set(ok)).await.unwrap();
    assert_eq!((res.applied, res.atomic), (4, false));
    let rows = fetch(&*s, &db, "e", |_| {}).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(
        ext(&rows[0], "b"),
        Value::Document(vec![("c".into(), Value::Int(9))])
    );
    assert!(rows[0].doc_get("a").is_none());
    assert_eq!(ext(&rows[2], "a"), Value::Ext(ExtValue::new("int64", "40")));
    // matched 0 documents -> ChangeFailed, earlier changes stay (not atomic)
    let err = s
        .apply_changes(set(vec![
            DocumentChange::Update {
                id: Value::Int(1),
                set: vec![FieldEdit {
                    path: vec!["k".into()],
                    value: Value::Int(1),
                }],
                unset: vec![],
            },
            DocumentChange::Update {
                id: Value::Int(12345),
                set: vec![FieldEdit {
                    path: vec!["k".into()],
                    value: Value::Int(2),
                }],
                unset: vec![],
            },
        ]))
        .await
        .unwrap_err();
    match err {
        Error::ChangeFailed { index, applied, .. } => assert_eq!((index, applied), (1, 1)),
        o => panic!("{o:?}"),
    }
    let rows = fetch(&*s, &db, "e", |_| {}).await;
    assert_eq!(ext(&rows[0], "k"), Value::Int(1));
    drop_db(&*s, &db).await;
}

#[tokio::test]
async fn editing_on_replica_set_is_atomic() {
    if !enabled() {
        return;
    }
    let s = connect(Srv::M7).await;
    let db = unique_db();
    run(
        &*s,
        &db,
        "db.e.insertMany([{_id: 1, a: 1}, {_id: 2, a: 2}])",
    )
    .await;
    let ed = s.editability(&coll_path(&db, "e")).await.unwrap();
    assert!(ed.atomic);
    let set = |changes| {
        ChangeSet::Documents(DocumentChangeSet {
            collection: coll_path(&db, "e"),
            changes,
        })
    };
    let err = s
        .apply_changes(set(vec![
            DocumentChange::Delete { id: Value::Int(1) },
            DocumentChange::Insert {
                document: DocInput::Ejson {
                    text: r#"{"_id": 2}"#.into(),
                },
            }, // duplicate key
        ]))
        .await
        .unwrap_err();
    match err {
        Error::ChangeFailed { index, applied, .. } => assert_eq!((index, applied), (1, 0)),
        o => panic!("{o:?}"),
    }
    let rows = fetch(&*s, &db, "e", |_| {}).await;
    assert_eq!(rows.len(), 2, "the delete was rolled back");
    let res = s
        .apply_changes(set(vec![
            DocumentChange::Delete { id: Value::Int(1) },
            DocumentChange::Insert {
                document: DocInput::Ejson {
                    text: r#"{"_id": 5}"#.into(),
                },
            },
        ]))
        .await
        .unwrap();
    assert!(res.atomic && res.applied == 2);
    assert_eq!(fetch(&*s, &db, "e", |_| {}).await.len(), 2);
    // views are not editable
    run(&*s, &db, r#"db.createView("v", "e", [])"#).await;
    let vp = ObjectPath::root()
        .child(ObjectKind::Database, &db)
        .child(ObjectKind::Folder, "views")
        .child(ObjectKind::View, "v");
    let ed = s.editability(&vp).await.unwrap();
    assert_eq!(ed.reason, Some(NotEditableReason::NotATable));
    drop_db(&*s, &db).await;
}

#[tokio::test]
async fn read_only_profile_blocks_edits() {
    if !enabled() {
        return;
    }
    let mut cfg = config(Srv::M44);
    cfg.read_only = true;
    let s = connect_cfg(&cfg).await.unwrap();
    let ed = s.editability(&coll_path("x", "y")).await.unwrap();
    assert_eq!(ed.reason, Some(NotEditableReason::ReadOnlyConnection));
    let err = s
        .apply_changes(ChangeSet::Documents(DocumentChangeSet {
            collection: coll_path("x", "y"),
            changes: vec![DocumentChange::Delete { id: Value::Int(1) }],
        }))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "readOnly");
}

#[tokio::test]
async fn long_query_is_cancelled_quickly() {
    if !enabled() {
        return;
    }
    for srv in [Srv::M44, Srv::M7] {
        let s: std::sync::Arc<dyn Session> = connect(srv).await.into();
        let db = unique_db();
        run(&*s, &db, "db.c.insertOne({a: 1})").await;
        let mut req = QueryRequest::new(r#"db.c.find({$where: "sleep(30000) || true"})"#);
        req.database = Some(db.clone());
        let id = req.query_id;
        let s2 = s.clone();
        let t0 = Instant::now();
        let handle = tokio::spawn(async move { execute_collect(&*s2, req).await });
        tokio::time::sleep(Duration::from_millis(500)).await;
        s.cancel(id).await.unwrap();
        let res = handle.await.unwrap();
        assert!(matches!(res, Err(Error::Cancelled)), "{srv:?}: {res:?}");
        assert!(t0.elapsed() < Duration::from_secs(8), "{:?}", t0.elapsed());
        // unknown id is fine, and the server reports the op is gone
        s.cancel(uuid::Uuid::new_v4()).await.unwrap();
        // Timeout path
        let mut req = QueryRequest::new(r#"db.c.find({$where: "sleep(30000) || true"})"#);
        req.database = Some(db.clone());
        req.timeout_ms = Some(800);
        let t0 = Instant::now();
        let res = execute_collect(&*s, req).await;
        assert!(matches!(res, Err(Error::Timeout)), "{res:?}");
        assert!(t0.elapsed() < Duration::from_secs(8));
        drop_db(&*s, &db).await;
    }
}

#[tokio::test]
async fn explain_returns_canonical_plan() {
    if !enabled() {
        return;
    }
    let s = connect(Srv::M44).await;
    let db = unique_db();
    run(
        &*s,
        &db,
        "db.x.insertMany([{a:1},{a:2}]); db.x.createIndex({a:1})",
    )
    .await;
    for (text, analyze) in [
        ("db.x.find({a: 1})", false),
        ("db.x.find({a: {$gt: 0}}).sort({a: 1})", true),
        ("db.x.aggregate([{$match: {a: 1}}])", true),
        ("db.x.updateOne({a: 1}, {$set: {b: 1}})", true),
        ("db.x.countDocuments({a: 1})", false),
    ] {
        let plan = s
            .explain(ExplainRequest {
                query_id: uuid::Uuid::new_v4(),
                text: text.into(),
                database: Some(db.clone()),
                analyze,
                options: Default::default(),
                timeout_ms: None,
            })
            .await
            .unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(plan.format, PlanFormat::MongoExplain);
        assert!(plan.text.contains("queryPlanner"), "{text}: {}", plan.text);
        assert_eq!(plan.analyzed, analyze);
        if analyze {
            assert!(plan.text.contains("executionStats"));
        }
    }
    // the explained update did not modify data
    let rows = fetch(&*s, &db, "x", |_| {}).await;
    assert!(rows.iter().all(|d| d.doc_get("b").is_none()));
    let e = s
        .explain(ExplainRequest {
            query_id: uuid::Uuid::new_v4(),
            text: "db.x.insertOne({})".into(),
            database: Some(db.clone()),
            analyze: false,
            options: Default::default(),
            timeout_ms: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind(), "unsupported");
    drop_db(&*s, &db).await;
}

#[tokio::test]
async fn schema_model_and_ddl() {
    if !enabled() {
        return;
    }
    let s = connect(Srv::M44).await;
    let db = unique_db();
    run(&*s, &db, r#"db.users.insertMany([{_id: ObjectId("65f0a1b2c3d4e5f607182931"), name: "a"}, {_id: ObjectId("65f0a1b2c3d4e5f607182932"), name: "b", age: 3}])"#).await;
    run(&*s, &db, r#"db.orders.insertOne({userId: ObjectId("65f0a1b2c3d4e5f607182930"), total: NumberDecimal("1.5")})"#).await;
    run(&*s, &db, "db.users.createIndex({name: 1}, {unique: true})").await;
    let dbp = ObjectPath::root().child(ObjectKind::Database, &db);
    let m = s
        .schema_model(SchemaModelRequest {
            scope: dbp.clone(),
            tables: vec![],
            include_views: false,
            include_comments: false,
            detail: ModelDetail::Full,
            sample_size: Some(100),
        })
        .await
        .unwrap();
    assert!(m.inferred && !m.truncated);
    let users = m.tables.iter().find(|t| t.name == "users").unwrap();
    let age = users.columns.iter().find(|c| c.name == "age").unwrap();
    assert!(age.nullable && age.extra.as_deref() == Some("seen 50%"));
    assert_eq!(users.unique_keys, vec![vec!["name".to_string()]]);
    assert_eq!(m.relations.len(), 1);
    assert!(m.relations[0].inferred && m.relations[0].to_table.last().unwrap().name == "users");
    // DDL re-creates the collection and its indexes in an empty database
    let items = s
        .generate_ddl(DdlRequest {
            scope: dbp.clone(),
            objects: vec![],
            options: Default::default(),
        })
        .await
        .unwrap();
    assert!(items.iter().all(|i| i.language == DdlLanguage::MongoShell));
    assert!(
        items
            .iter()
            .any(|i| i.section == DdlSection::PostData && i.sql.contains("createIndex"))
    );
    let target = unique_db();
    for i in &items {
        run(&*s, &target, &i.sql).await;
    }
    let again = s
        .generate_ddl(DdlRequest {
            scope: ObjectPath::root().child(ObjectKind::Database, &target),
            objects: vec![],
            options: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        again.iter().map(|i| &i.sql).collect::<Vec<_>>(),
        items.iter().map(|i| &i.sql).collect::<Vec<_>>()
    );
    drop_db(&*s, &db).await;
    drop_db(&*s, &target).await;
}

#[tokio::test]
async fn lease_transactions_pending_results_and_bulk() {
    if !enabled() {
        return;
    }
    let s = connect(Srv::M7).await;
    let db = unique_db();
    run(
        &*s,
        &db,
        "db.t.insertMany([{_id: 1},{_id: 2},{_id: 3},{_id: 4},{_id: 5}])",
    )
    .await;
    let lease = s
        .open_lease(LeaseOptions {
            database: Some(db.clone()),
            schema: None,
            auto_commit: true,
        })
        .await
        .unwrap();
    // pending result: 3 fetch_more rounds
    let mut sink = CollectingSink::new();
    let mut req = QueryRequest::new("db.t.find({}).sort({_id: 1})");
    req.max_rows = Some(2);
    let qid = req.query_id;
    lease.execute(req, &mut sink).await.unwrap();
    assert_eq!(lease.state().pending_result, Some(qid));
    let mut total = 2;
    for expect_more in [true, false] {
        let mut sink = CollectingSink::new();
        let mut counter = 0;
        lease.fetch_more(qid, 2, None, &mut sink).await.unwrap();
        for r in sink.into_results() {
            if let StatementOutcome::Rows(rs) = r.outcome {
                counter += rs.rows.len();
                assert_eq!(rs.truncated, expect_more);
            }
        }
        total += counter;
    }
    assert_eq!(total, 5);
    assert!(lease.state().pending_result.is_none());
    assert_eq!(
        lease
            .fetch_more(qid, 1, None, &mut CollectingSink::new())
            .await
            .unwrap_err()
            .kind(),
        "notFound"
    );
    // manual transaction: rollback hides the insert from others, commit shows it
    lease.set_auto_commit(false).await.unwrap();
    let mut sink = CollectingSink::new();
    let mut req = QueryRequest::new("db.t.insertOne({_id: 100})");
    lease.execute(req.clone(), &mut sink).await.unwrap();
    req.query_id = uuid::Uuid::new_v4();
    assert_eq!(lease.state().transaction, TxStatus::Active);
    assert_eq!(
        fetch(&*s, &db, "t", |_| {}).await.len(),
        5,
        "uncommitted insert is invisible"
    );
    lease.rollback().await.unwrap();
    assert_eq!(lease.state().transaction, TxStatus::Idle);
    lease
        .execute(
            QueryRequest::new("db.t.insertOne({_id: 101})"),
            &mut CollectingSink::new(),
        )
        .await
        .unwrap();
    lease.commit().await.unwrap();
    assert_eq!(fetch(&*s, &db, "t", |_| {}).await.len(), 6);
    assert!(lease.set_auto_commit(true).await.is_ok());
    // bulk insert (error / skip / update) and stream_table
    let rows = |ids: &[i64]| -> Vec<Vec<EditValue>> {
        ids.iter()
            .map(|i| {
                vec![
                    EditValue::Value(Value::Int(*i)),
                    EditValue::Text(format!("n{i}")),
                ]
            })
            .collect()
    };
    let r = lease
        .bulk_insert(BulkInsertRequest {
            table: coll_path(&db, "t"),
            columns: vec!["_id".into(), "name".into()],
            rows: rows(&[200, 201]),
            on_conflict: OnConflict::Error,
            conflict_columns: vec![],
        })
        .await
        .unwrap();
    assert_eq!(r.inserted, 2);
    let r = lease
        .bulk_insert(BulkInsertRequest {
            table: coll_path(&db, "t"),
            columns: vec!["_id".into(), "name".into()],
            rows: rows(&[201, 202]),
            on_conflict: OnConflict::Skip,
            conflict_columns: vec![],
        })
        .await
        .unwrap();
    assert_eq!((r.inserted, r.skipped), (1, 1));
    let r = lease
        .bulk_insert(BulkInsertRequest {
            table: coll_path(&db, "t"),
            columns: vec!["_id".into(), "name".into()],
            rows: rows(&[202, 203]),
            on_conflict: OnConflict::Update,
            conflict_columns: vec![],
        })
        .await
        .unwrap();
    assert_eq!((r.inserted, r.updated), (1, 1));
    let mut sink = CollectingSink::new();
    lease
        .stream_table(
            TableStreamRequest {
                query_id: uuid::Uuid::new_v4(),
                table: coll_path(&db, "t"),
                columns: vec![],
                filters: vec![],
                raw_where: None,
                order_by_key: true,
                batch_size: Some(3),
            },
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(docs(&sink.into_results()[0]).len(), 10);
    // close with an open transaction reports a rollback
    lease.set_auto_commit(false).await.unwrap();
    lease
        .execute(
            QueryRequest::new("db.t.insertOne({_id: 300})"),
            &mut CollectingSink::new(),
        )
        .await
        .unwrap();
    let rep = lease.close().await.unwrap();
    assert!(rep.rolled_back);
    assert_eq!(
        fetch(&*s, &db, "t", |r| r.filters = vec![ColumnFilter {
            column: "_id".into(),
            op: FilterOp::Eq,
            values: vec!["300".into()]
        }])
        .await
        .len(),
        0
    );
    drop_db(&*s, &db).await;
}

#[tokio::test]
async fn lease_on_standalone_has_no_transactions() {
    if !enabled() {
        return;
    }
    let s = connect(Srv::M44).await;
    let lease = s
        .open_lease(LeaseOptions {
            database: None,
            schema: None,
            auto_commit: true,
        })
        .await
        .unwrap();
    assert_eq!(
        lease
            .begin(TxOptions::default())
            .await
            .unwrap_err()
            .to_string(),
        "unsupported: transactions_standalone"
    );
    assert_eq!(
        lease.set_auto_commit(false).await.unwrap_err().kind(),
        "unsupported"
    );
    lease.ping().await.unwrap();
    lease.close().await.unwrap();
}

#[tokio::test]
async fn review_guards_in_the_driver() {
    if !enabled() {
        return;
    }
    let s = connect(Srv::M44).await;
    let db = unique_db();
    run(
        &*s,
        &db,
        "db.g.insertMany([{_id: 1, a: -5}, {_id: 2, a: 5}])",
    )
    .await;
    // Chained calls never turn a write into something else, and nothing is executed.
    for text in [
        "db.g.deleteMany({}).explain()",
        "db.g.updateMany({}, {$set: {x: 1}}).limit(1)",
    ] {
        let mut req = QueryRequest::new(text);
        req.database = Some(db.clone());
        let e = execute_collect(&*s, req).await.unwrap_err();
        assert_eq!(e.kind(), "query", "{text}: {e}");
    }
    // A null / missing filter on a write is refused.
    for text in [
        "db.g.deleteMany()",
        "db.g.deleteOne(null)",
        "db.g.updateMany(null, {$set: {x: 1}})",
    ] {
        let mut req = QueryRequest::new(text);
        req.database = Some(db.clone());
        assert!(execute_collect(&*s, req).await.is_err(), "{text}");
    }
    assert_eq!(
        fetch(&*s, &db, "g", |_| {}).await.len(),
        2,
        "nothing was deleted"
    );
    // `.explain()` on cursors explains instead of running ($out must not write).
    let r = run(
        &*s,
        &db,
        "db.g.aggregate([{$match: {a: 5}}, {$out: 'copy'}]).explain()",
    )
    .await;
    assert!(
        docs(&r[0])[0].doc_get("stages").is_some()
            || docs(&r[0])[0].doc_get("queryPlanner").is_some()
    );
    let r = run(&*s, &db, "show collections").await;
    assert_eq!(docs_names(&r[0]), ["g"], "$out did not create a collection");
    // Negative Int32 keeps its type through the EJSON insert path (plain and tagged forms).
    let ins = ChangeSet::Documents(DocumentChangeSet {
        collection: coll_path(&db, "g"),
        changes: vec![DocumentChange::Insert {
            document: DocInput::Ejson {
                text:
                    r#"{"_id": 3, "a": -7, "b": {"$numberInt": "-8"}, "c": {"$numberLong": "-9"}}"#
                        .into(),
            },
        }],
    });
    s.apply_changes(ins).await.unwrap();
    let rows = fetch(&*s, &db, "g", |r| {
        r.filters = vec![ColumnFilter {
            column: "_id".into(),
            op: FilterOp::Eq,
            values: vec!["3".into()],
        }]
    })
    .await;
    assert_eq!(ext(&rows[0], "a"), Value::Int(-7));
    assert_eq!(ext(&rows[0], "b"), Value::Int(-8));
    assert_eq!(ext(&rows[0], "c"), Value::Ext(ExtValue::new("int64", "-9")));
    // Unsafe edits are refused.
    let bad_path = ChangeSet::Documents(DocumentChangeSet {
        collection: coll_path(&db, "g"),
        changes: vec![DocumentChange::Update {
            id: Value::Int(1),
            set: vec![FieldEdit {
                path: vec!["a.b".into()],
                value: Value::Int(1),
            }],
            unset: vec![],
        }],
    });
    assert_eq!(s.apply_changes(bad_path).await.unwrap_err().kind(), "query");
    // Commands tagged with a comment still work (drop / create / rename / dropDatabase).
    run(&*s, &db, "db.createCollection('t1'); db.t1.insertOne({a: 1}); db.t1.renameCollection('t2'); db.t2.drop()").await;
    drop_db(&*s, &db).await;
}

#[tokio::test]
async fn prefer_mode_reports_the_downgrade_and_never_hides_other_failures() {
    if !enabled() {
        return;
    }
    // Plain server + prefer: falls back and says so; TLS server + prefer: stays on TLS.
    let mut cfg = config(Srv::M44);
    cfg.tls.mode = TlsMode::Prefer;
    let s = connect_cfg(&cfg).await.unwrap();
    assert_eq!(
        s.server_info().await.unwrap().extra["tls"],
        "false (fallback)"
    );
    let lease = s
        .open_lease(LeaseOptions {
            database: None,
            schema: None,
            auto_commit: true,
        })
        .await
        .unwrap();
    lease.ping().await.unwrap();
    let mut cfg = config(Srv::M7);
    cfg.tls.mode = TlsMode::Prefer;
    let s = connect_cfg(&cfg).await.unwrap();
    assert_eq!(s.server_info().await.unwrap().extra["tls"], "true");
    // A refused connection is an error, not a silent plain retry.
    let mut cfg = config(Srv::M44);
    cfg.tls.mode = TlsMode::Prefer;
    cfg.port = 1;
    cfg.connect_timeout_ms = Some(3000);
    let t0 = Instant::now();
    let err = connect_cfg(&cfg).await.err().expect("must fail");
    assert_eq!(err.kind(), "connection", "{err}");
    assert!(t0.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn sibling_database_statements_run_on_that_database() {
    if !enabled() {
        return;
    }
    for srv in [Srv::M44, Srv::M7] {
        let s = connect(srv).await;
        let (here, other) = (unique_db(), unique_db());
        // `use`-less request on `here`, statement addressed to `other`.
        run(
            &*s,
            &here,
            &format!("db.getSiblingDB('{other}').c.insertOne({{a: 1}})"),
        )
        .await;
        assert_eq!(
            fetch(&*s, &other, "c", |_| {}).await.len(),
            1,
            "{srv:?}: written to the other db"
        );
        let r = run(&*s, &here, "show collections").await;
        assert!(
            docs_names(&r[0]).is_empty(),
            "nothing was written to the current db"
        );
        // explain honours it too.
        let plan = s
            .explain(ExplainRequest {
                query_id: uuid::Uuid::new_v4(),
                text: format!("db.getSiblingDB('{other}').c.find({{a: 1}})"),
                database: Some(here.clone()),
                analyze: true,
                options: Default::default(),
                timeout_ms: None,
            })
            .await
            .unwrap();
        assert!(plan.text.contains(&format!("{other}.c")), "{}", plan.text);
        // Inside a lease (and its transaction on a replica set).
        let lease = s
            .open_lease(LeaseOptions {
                database: Some(here.clone()),
                schema: None,
                auto_commit: srv == Srv::M44,
            })
            .await
            .unwrap();
        lease
            .execute(
                QueryRequest::new(format!("db.getSiblingDB('{other}').c.insertOne({{a: 2}})")),
                &mut CollectingSink::new(),
            )
            .await
            .unwrap();
        if srv == Srv::M7 {
            lease.commit().await.unwrap();
        }
        assert_eq!(fetch(&*s, &other, "c", |_| {}).await.len(), 2);
        lease.close().await.unwrap();
        // Dropping a sibling database drops that one only.
        run(&*s, &here, "db.keep.insertOne({a: 1})").await;
        run(
            &*s,
            &here,
            &format!("db.getSiblingDB('{other}').dropDatabase()"),
        )
        .await;
        assert!(fetch(&*s, &other, "c", |_| {}).await.is_empty());
        assert_eq!(fetch(&*s, &here, "keep", |_| {}).await.len(), 1);
        drop_db(&*s, &here).await;
    }
}

#[tokio::test]
async fn read_only_profile_blocks_scripted_writes_including_sibling_databases() {
    if !enabled() {
        return;
    }
    let rw = connect(Srv::M44).await;
    let db = unique_db();
    run(&*rw, &db, "db.c.insertOne({a: 1})").await;
    let mut cfg = config(Srv::M44);
    cfg.read_only = true;
    let ro = connect_cfg(&cfg).await.unwrap();
    for text in [
        format!("db.getSiblingDB('{db}').dropDatabase()"),
        format!("db.getSiblingDB('{db}').c.deleteMany({{}})"),
        "db.c.insertOne({a: 2})".to_string(),
        "db.c.drop()".to_string(),
        "db.c.aggregate([{$out: 'x'}])".to_string(),
    ] {
        let mut req = QueryRequest::new(text.clone());
        req.database = Some(db.clone());
        let e = execute_collect(&*ro, req).await.unwrap_err();
        assert_eq!(e.kind(), "readOnly", "{text}: {e}");
    }
    // Reads and show still work.
    let mut req = QueryRequest::new("db.c.find({}); show collections");
    req.database = Some(db.clone());
    assert_eq!(execute_collect(&*ro, req).await.unwrap().len(), 2);
    assert_eq!(
        fetch(&*rw, &db, "c", |_| {}).await.len(),
        1,
        "data untouched"
    );
    drop_db(&*rw, &db).await;
}
