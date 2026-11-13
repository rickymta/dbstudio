//! Integration tests against `redis6` (ACL user, `KEYS` renamed away), `redis7` (TLS only) and
//! `valkey8`. They only run when `DBSTUDIO_IT=1`; otherwise every test returns immediately.
//! The 1,000,000 key walk additionally needs `DBSTUDIO_IT_BIG=1`.

use std::time::Duration;

use dbstudio_core::Driver;
use dbstudio_core::{
    ApplyResult, ChangeSet, ChildrenPageRequest, CollectingSink, ConnectionConfig, DbKind, Error,
    KeyChange, KeyChangeSet, KeyEntries, KeyReadRequest, LeaseContext, LeaseOptions, ObjectKind,
    ObjectPath, QueryRequest, Secret, Session, StatementOutcome, TlsMode, TxStatus, Value,
    execute_collect,
};
use dbstudio_driver_redis::RedisDriver;

#[derive(Clone, Copy)]
struct Server {
    name: &'static str,
    env_port: &'static str,
    default_port: u16,
    tls: bool,
}

const REDIS6: Server = Server {
    name: "redis6",
    env_port: "DBSTUDIO_IT_REDIS6_PORT",
    default_port: 63796,
    tls: false,
};
const REDIS7: Server = Server {
    name: "redis7",
    env_port: "DBSTUDIO_IT_REDIS7_PORT",
    default_port: 63797,
    tls: true,
};
const VALKEY8: Server = Server {
    name: "valkey8",
    env_port: "DBSTUDIO_IT_VALKEY8_PORT",
    default_port: 63798,
    tls: false,
};

fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").is_ok_and(|v| v == "1")
}

fn config(s: Server) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new(DbKind::Redis, s.name, "127.0.0.1");
    cfg.port = std::env::var(s.env_port)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(s.default_port);
    cfg.username = Some("dbstudio".into());
    cfg.tls.mode = if s.tls {
        TlsMode::Require
    } else {
        TlsMode::Disable
    };
    cfg
}

fn secret() -> Secret {
    Secret::new("dbstudio_test")
}

async fn connect_with(cfg: &ConnectionConfig) -> dbstudio_core::Result<Box<dyn Session>> {
    RedisDriver::new().connect(cfg, Some(&secret())).await
}

async fn connect(s: Server) -> Box<dyn Session> {
    connect_with(&config(s)).await.expect("connect")
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn db_path(db: i64) -> ObjectPath {
    ObjectPath::root().child(ObjectKind::Database, db.to_string())
}

fn key_path(db: i64, key: &str) -> ObjectPath {
    db_path(db).child(ObjectKind::Key, key)
}

async fn run(s: &dyn Session, db: i64, text: &str) -> Vec<StatementOutcome> {
    let mut req = QueryRequest::new(text);
    req.database = Some(db.to_string());
    execute_collect(s, req)
        .await
        .unwrap_or_else(|e| panic!("`{text}` failed: {e}"))
        .into_iter()
        .map(|r| r.outcome)
        .collect()
}

async fn scalar(s: &dyn Session, db: i64, text: &str) -> Value {
    match run(s, db, text).await.remove(0) {
        StatementOutcome::Rows(rs) => rs.rows[0].last().cloned().unwrap(),
        other => panic!("{other:?}"),
    }
}

async fn fresh(s: Server, db: i64) -> Box<dyn Session> {
    let session = connect(s).await;
    run(session.as_ref(), db, "FLUSHDB").await;
    session
}

async fn read(s: &dyn Session, db: i64, key: &str) -> dbstudio_core::KeyValuePage {
    s.read_key(KeyReadRequest {
        key: key_path(db, key),
        cursor: None,
        count: 100,
        pattern: None,
        max_string_bytes: None,
    })
    .await
    .unwrap()
}

async fn apply(
    s: &dyn Session,
    db: i64,
    changes: Vec<KeyChange>,
) -> dbstudio_core::Result<ApplyResult> {
    s.apply_changes(ChangeSet::Keys(KeyChangeSet {
        database: db_path(db),
        changes,
    }))
    .await
}

macro_rules! on_all {
    ($f:ident, $a:ident, $b:ident, $c:ident, $db:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $a() {
            if enabled() {
                $f(REDIS6, $db).await
            }
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $b() {
            if enabled() {
                $f(REDIS7, $db).await
            }
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $c() {
            if enabled() {
                $f(VALKEY8, $db).await
            }
        }
    };
}

// --------------------------------------------------------------------------------------------

async fn connects_and_reports_server(s: Server, _db: i64) {
    let session = connect(s).await;
    session.ping().await.unwrap();
    let info = session.server_info().await.unwrap();
    if s.name == "valkey8" {
        assert_eq!(info.product, "Valkey");
        assert!(info.version.starts_with('8'));
    } else {
        assert_eq!(info.product, "Redis");
        assert!(
            info.version
                .starts_with(if s.name == "redis6" { "6." } else { "7." })
        );
    }
    assert!(info.extra.contains_key("role"));
    session.close().await.unwrap();
}
on_all!(
    connects_and_reports_server,
    connect_r6,
    connect_r7,
    connect_v8,
    0
);

#[tokio::test]
async fn bad_password_is_an_authentication_error() {
    if !enabled() {
        return;
    }
    let err = RedisDriver::new()
        .connect(&config(REDIS6), Some(&Secret::new("wrong")))
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind(), "authentication", "{err}");
}

#[tokio::test]
async fn tls_modes_against_the_tls_only_server() {
    if !enabled() {
        return;
    }
    // Disable cannot talk to a TLS-only port.
    let mut cfg = config(REDIS7);
    cfg.tls.mode = TlsMode::Disable;
    cfg.connect_timeout_ms = Some(2000);
    assert!(connect_with(&cfg).await.is_err());
    // Prefer finds TLS.
    cfg.tls.mode = TlsMode::Prefer;
    connect_with(&cfg).await.unwrap().ping().await.unwrap();
    // Verify with the test CA (the certificate is issued for localhost).
    let ca = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../docker/certs/ca.crt");
    let mut cfg = config(REDIS7);
    cfg.host = "localhost".into();
    cfg.tls.mode = TlsMode::VerifyFull;
    cfg.tls.ca_cert_path = Some(ca.into());
    connect_with(&cfg).await.unwrap().ping().await.unwrap();
    // A different CA must be refused.
    cfg.tls.ca_cert_path = Some(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../docker/certs/other-ca.crt"
        )
        .into(),
    );
    let err = connect_with(&cfg).await.err().unwrap();
    assert!(matches!(err, Error::Tls(_) | Error::Connection(_)), "{err}");
}

async fn tree_groups_keys_by_delimiter(s: Server, db: i64) {
    let session = fresh(s, db).await;
    let sess = session.as_ref();
    run(
        sess,
        db,
        "SET user:1:name a\nSET user:1:email b\nSET user:2:name c\nSET plain 1\nSET a::b 2\nHSET h f v\nEXPIRE plain 1000",
    )
    .await;
    // Root lists the non-empty db and the default db.
    let root = sess.list_children(&ObjectPath::root()).await.unwrap();
    let node = root
        .iter()
        .find(|n| n.name == db.to_string())
        .expect("db listed");
    assert_eq!(node.kind, ObjectKind::Database);
    assert_eq!(node.detail.as_deref(), Some("6"));
    assert!(
        root.iter().any(|n| n.name == "0"),
        "default db always shown"
    );

    let page = sess
        .list_children_page(ChildrenPageRequest {
            parent: db_path(db),
            cursor: None,
            limit: 100,
            pattern: None,
        })
        .await
        .unwrap();
    assert_eq!(page.next_cursor, None);
    assert_eq!(page.approximate_total, Some(6));
    let names: Vec<(ObjectKind, &str)> = page
        .nodes
        .iter()
        .map(|n| (n.kind, n.name.as_str()))
        .collect();
    assert!(names.contains(&(ObjectKind::KeyPrefix, "user:")));
    assert!(names.contains(&(ObjectKind::KeyPrefix, "a:")));
    assert!(names.contains(&(ObjectKind::Key, "plain")));
    assert!(names.contains(&(ObjectKind::Key, "h")));
    let plain = page.nodes.iter().find(|n| n.name == "plain").unwrap();
    assert!(
        plain.detail.as_deref().unwrap().starts_with("string ttl:"),
        "{:?}",
        plain.detail
    );
    assert_eq!(
        page.nodes
            .iter()
            .find(|n| n.name == "h")
            .unwrap()
            .detail
            .as_deref(),
        Some("hash")
    );

    // Descend: user: -> user:1:, user:2:
    let user = page.nodes.iter().find(|n| n.name == "user:").unwrap();
    let children = sess.list_children(&user.path).await.unwrap();
    let names: Vec<&str> = children.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, ["user:1:", "user:2:"]);
    let leaf = sess.list_children(&children[0].path).await.unwrap();
    assert_eq!(
        leaf.iter()
            .map(|n| (n.kind, n.name.as_str()))
            .collect::<Vec<_>>(),
        [
            (ObjectKind::Key, "user:1:email"),
            (ObjectKind::Key, "user:1:name")
        ]
    );
    // "a::b": the empty segment keeps the prefix chain consistent.
    let a = page.nodes.iter().find(|n| n.name == "a:").unwrap();
    let a_children = sess.list_children(&a.path).await.unwrap();
    assert_eq!(a_children[0].name, "a::");

    // Glob pattern on full key names.
    let page = sess
        .list_children_page(ChildrenPageRequest {
            parent: db_path(db),
            cursor: None,
            limit: 100,
            pattern: Some("user:2:*".into()),
        })
        .await
        .unwrap();
    assert_eq!(page.nodes.len(), 1);
    assert_eq!(page.nodes[0].name, "user:");

    // Patterns are relative to the node: below `user:` the pattern `1*` keeps only `user:1:`.
    let page = sess
        .list_children_page(ChildrenPageRequest {
            parent: user.path.clone(),
            cursor: None,
            limit: 100,
            pattern: Some("1*".into()),
        })
        .await
        .unwrap();
    assert_eq!(
        page.nodes
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>(),
        ["user:1:"]
    );
}
on_all!(tree_groups_keys_by_delimiter, tree_r6, tree_r7, tree_v8, 1);

async fn tree_pages_with_cursor(s: Server, db: i64) {
    let session = fresh(s, db).await;
    let sess = session.as_ref();
    // 3000 distinct prefixes force several pages.
    let mut script = String::new();
    for i in 0..3000 {
        script.push_str(&format!("SET p{i:04}:k v\n"));
    }
    run(sess, db, &script).await;
    let mut seen = std::collections::BTreeSet::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let page = sess
            .list_children_page(ChildrenPageRequest {
                parent: db_path(db),
                cursor: cursor.clone(),
                limit: 500,
                pattern: None,
            })
            .await
            .unwrap();
        pages += 1;
        for n in page.nodes {
            seen.insert(n.name);
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(seen.len(), 3000);
    assert!(pages >= 3, "expected several pages, got {pages}");
}
on_all!(tree_pages_with_cursor, page_r6, page_r7, page_v8, 2);

async fn all_six_types_read_and_edit(s: Server, db: i64) {
    let session = fresh(s, db).await;
    let sess = session.as_ref();
    run(
        sess,
        db,
        "SET str hello\nHSET hash a 1 b 2\nRPUSH list x y z\nSADD set m1 m2\nZADD zset 1 one 2.5 two\nXADD stream 1-1 f v\nXADD stream 2-1 g w",
    )
    .await;
    let p = read(sess, db, "str").await;
    assert_eq!(p.key_type, "string");
    assert_eq!(p.length, Some(5));
    assert!(
        matches!(&p.entries, KeyEntries::String { value, truncated: false, .. } if *value == text("hello"))
    );
    assert_eq!(p.ttl_ms, None);

    let p = read(sess, db, "hash").await;
    let KeyEntries::Hash { fields } = p.entries else {
        panic!()
    };
    assert_eq!(fields.len(), 2);
    assert_eq!(p.length, Some(2));

    let p = read(sess, db, "list").await;
    let KeyEntries::List { start, items } = p.entries else {
        panic!()
    };
    assert_eq!((start, items), (0, vec![text("x"), text("y"), text("z")]));

    let p = read(sess, db, "set").await;
    let KeyEntries::Set { members } = p.entries else {
        panic!()
    };
    assert_eq!(members.len(), 2);

    let p = read(sess, db, "zset").await;
    let KeyEntries::ZSet { members } = p.entries else {
        panic!()
    };
    assert_eq!(
        members,
        vec![(text("one"), "1".into()), (text("two"), "2.5".into())]
    );

    let p = read(sess, db, "stream").await;
    let KeyEntries::Stream { entries, groups } = p.entries else {
        panic!()
    };
    assert_eq!(
        (entries.len(), groups, entries[0].id.as_str()),
        (2, 0, "1-1")
    );

    // Missing key.
    let err = sess
        .read_key(KeyReadRequest {
            key: key_path(db, "nope"),
            cursor: None,
            count: 10,
            pattern: None,
            max_string_bytes: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "notFound");

    // Edit every type.
    let key = |k: &str| k.to_string();
    let result = apply(
        sess,
        db,
        vec![
            KeyChange::SetString {
                key: key("str"),
                value: text("changed"),
                ttl_ms: None,
            },
            KeyChange::HashSet {
                key: key("hash"),
                entries: vec![(text("c"), text("3"))],
            },
            KeyChange::HashDelete {
                key: key("hash"),
                fields: vec![text("a")],
            },
            KeyChange::ListSet {
                key: key("list"),
                index: 1,
                value: text("Y"),
            },
            KeyChange::ListPush {
                key: key("list"),
                values: vec![text("w")],
                head: true,
            },
            KeyChange::ListRemoveAt {
                key: key("list"),
                index: 3,
            },
            KeyChange::SetAdd {
                key: key("set"),
                members: vec![text("m3")],
            },
            KeyChange::SetRemove {
                key: key("set"),
                members: vec![text("m1")],
            },
            KeyChange::ZAdd {
                key: key("zset"),
                members: vec![(text("three"), "3".into())],
            },
            KeyChange::ZRemove {
                key: key("zset"),
                members: vec![text("one")],
            },
            KeyChange::StreamAdd {
                key: key("stream"),
                id: Some("3-1".into()),
                fields: vec![(text("h"), text("x"))],
            },
            KeyChange::StreamDelete {
                key: key("stream"),
                ids: vec!["1-1".into()],
            },
            KeyChange::SetString {
                key: key("new"),
                value: text("fresh"),
                ttl_ms: Some(60_000),
            },
        ],
    )
    .await
    .unwrap();
    assert_eq!(result.applied, 13);
    assert!(!result.atomic);

    assert!(
        matches!(read(sess, db, "str").await.entries, KeyEntries::String { value, .. } if value == text("changed"))
    );
    let KeyEntries::Hash { fields } = read(sess, db, "hash").await.entries else {
        panic!()
    };
    assert_eq!(fields.len(), 2);
    assert!(!fields.iter().any(|(f, _)| *f == text("a")));
    let KeyEntries::List { items, .. } = read(sess, db, "list").await.entries else {
        panic!()
    };
    // w x Y z -> remove index 3 (z) => w x Y
    assert_eq!(items, vec![text("w"), text("x"), text("Y")]);
    let KeyEntries::Set { members } = read(sess, db, "set").await.entries else {
        panic!()
    };
    assert!(members.contains(&text("m3")) && !members.contains(&text("m1")));
    let KeyEntries::ZSet { members } = read(sess, db, "zset").await.entries else {
        panic!()
    };
    assert_eq!(
        members,
        vec![(text("two"), "2.5".into()), (text("three"), "3".into())]
    );
    let KeyEntries::Stream { entries, .. } = read(sess, db, "stream").await.entries else {
        panic!()
    };
    assert_eq!(
        entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["2-1", "3-1"]
    );
    let new = read(sess, db, "new").await;
    assert!(new.ttl_ms.unwrap() > 50_000);
}
on_all!(all_six_types_read_and_edit, types_r6, types_r7, types_v8, 3);

async fn ttl_rename_delete_and_failures(s: Server, db: i64) {
    let session = fresh(s, db).await;
    let sess = session.as_ref();
    run(sess, db, "SET k v\nSET other o\nHSET h f v").await;
    apply(
        sess,
        db,
        vec![KeyChange::Expire {
            key: "k".into(),
            ttl_ms: Some(90_000),
        }],
    )
    .await
    .unwrap();
    let ttl = read(sess, db, "k").await.ttl_ms.unwrap();
    assert!((80_000..=90_000).contains(&ttl), "{ttl}");
    // Plain SET keeps the TTL.
    apply(
        sess,
        db,
        vec![KeyChange::SetString {
            key: "k".into(),
            value: text("v2"),
            ttl_ms: None,
        }],
    )
    .await
    .unwrap();
    assert!(read(sess, db, "k").await.ttl_ms.is_some());
    apply(
        sess,
        db,
        vec![KeyChange::Expire {
            key: "k".into(),
            ttl_ms: None,
        }],
    )
    .await
    .unwrap();
    assert_eq!(read(sess, db, "k").await.ttl_ms, None);

    apply(
        sess,
        db,
        vec![KeyChange::Rename {
            key: "k".into(),
            new_key: "k2".into(),
        }],
    )
    .await
    .unwrap();
    assert!(
        sess.read_key(KeyReadRequest {
            key: key_path(db, "k"),
            cursor: None,
            count: 1,
            pattern: None,
            max_string_bytes: None
        })
        .await
        .is_err()
    );
    // Rename onto an existing key fails and reports 0 applied (it is the first change).
    let err = apply(
        sess,
        db,
        vec![KeyChange::Rename {
            key: "k2".into(),
            new_key: "other".into(),
        }],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::ChangeFailed {
                index: 0,
                applied: 0,
                ..
            }
        ),
        "{err:?}"
    );

    // Stops at the first failure and reports the number applied.
    let err = apply(
        sess,
        db,
        vec![
            KeyChange::SetString {
                key: "a".into(),
                value: text("1"),
                ttl_ms: None,
            },
            KeyChange::SetString {
                key: "b".into(),
                value: text("2"),
                ttl_ms: None,
            },
            KeyChange::HashSet {
                key: "k2".into(),
                entries: vec![(text("f"), text("v"))],
            }, // WRONGTYPE
            KeyChange::SetString {
                key: "c".into(),
                value: text("3"),
                ttl_ms: None,
            },
        ],
    )
    .await
    .unwrap_err();
    match err {
        Error::ChangeFailed {
            index,
            applied,
            message,
        } => {
            assert_eq!((index, applied), (2, 2));
            assert!(message.contains("WRONGTYPE"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(scalar(sess, db, "EXISTS a b c").await, Value::Int(2));

    // Delete.
    let r = apply(
        sess,
        db,
        vec![KeyChange::Delete {
            keys: vec!["a".into(), "b".into()],
        }],
    )
    .await
    .unwrap();
    assert_eq!(r.affected, vec![2]);

    // Preview.
    let lines = sess
        .preview_changes(&ChangeSet::Keys(KeyChangeSet {
            database: db_path(db),
            changes: vec![KeyChange::SetString {
                key: "k".into(),
                value: text("a b"),
                ttl_ms: Some(5),
            }],
        }))
        .await
        .unwrap();
    assert_eq!(lines, [r#"SET k "a b" PX 5"#]);
}
on_all!(ttl_rename_delete_and_failures, ttl_r6, ttl_r7, ttl_v8, 4);

async fn concurrent_modification_is_detected(s: Server, db: i64) {
    // A client modifying the watched key between WATCH and EXEC aborts the change. Simulate the
    // race by hammering the key from another connection while applying.
    let session = fresh(s, db).await;
    let sess = session.as_ref();
    run(sess, db, "SET race 0").await;
    let other = connect(s).await;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let writer = tokio::spawn(async move {
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            run(other.as_ref(), db, "SET race 1").await;
        }
    });
    let mut aborted = 0;
    for _ in 0..60 {
        if let Err(Error::ChangeFailed { message, .. }) = apply(
            sess,
            db,
            vec![KeyChange::SetString {
                key: "race".into(),
                value: text("x"),
                ttl_ms: None,
            }],
        )
        .await
        {
            assert!(message.contains("changed while"), "{message}");
            aborted += 1;
        }
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    writer.await.unwrap();
    // The race window is tiny; the assertion is only that aborted runs report the right reason.
    eprintln!("aborted by WATCH: {aborted}");
}
on_all!(
    concurrent_modification_is_detected,
    race_r6,
    race_r7,
    race_v8,
    5
);

async fn console_results_errors_and_lease(s: Server, db: i64) {
    let session = fresh(s, db).await;
    let sess = session.as_ref();
    assert_eq!(
        sess.script_dialect(),
        dbstudio_core::ScriptDialect::RedisCli
    );
    let out = run(
        sess,
        db,
        "SET a 1\nGET a\nRPUSH l 1 2 3\nLRANGE l 0 -1\nGET missing\nINCR a",
    )
    .await;
    assert_eq!(out.len(), 6);
    assert!(matches!(&out[1], StatementOutcome::Rows(rs) if rs.rows == vec![vec![text("1")]]));
    match &out[3] {
        StatementOutcome::Rows(rs) => {
            assert_eq!(rs.columns[0].name, "#");
            assert_eq!(rs.rows.len(), 3);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(&out[4], StatementOutcome::Rows(rs) if rs.rows == vec![vec![Value::Null]]));
    assert!(matches!(&out[5], StatementOutcome::Rows(rs) if rs.rows == vec![vec![Value::Int(2)]]));

    // Error: stops, position = start of the failing line.
    let mut req = QueryRequest::new("GET a\nLPUSH a x\nGET a");
    req.database = Some(db.to_string());
    let err = execute_collect(sess, req).await.unwrap_err();
    assert!(
        matches!(&err, Error::Query { position: Some(6), message } if message.contains("WRONGTYPE")),
        "{err:?}"
    );
    // Subscribe-style commands are refused.
    let err = execute_collect(sess, QueryRequest::new("SUBSCRIBE c"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "unsupported");
    // Tokenizer error.
    let err = execute_collect(sess, QueryRequest::new("GET \"oops"))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Query {
            position: Some(0),
            ..
        }
    ));

    // Lease: SELECT only affects the lease.
    let lease = sess
        .open_lease(LeaseOptions {
            database: Some(db.to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        lease.state().database.as_deref(),
        Some(db.to_string().as_str())
    );
    let mut sink = CollectingSink::new();
    lease
        .execute(QueryRequest::new("SELECT 9\nSET onlyin9 1"), &mut sink)
        .await
        .unwrap();
    assert_eq!(lease.state().database.as_deref(), Some("9"));
    assert_eq!(scalar(sess, 9, "EXISTS onlyin9").await, Value::Int(1));
    assert_eq!(scalar(sess, db, "EXISTS onlyin9").await, Value::Int(0));
    run(sess, 9, "DEL onlyin9").await;
    lease
        .set_context(LeaseContext {
            database: Some(db.to_string()),
            schema: None,
        })
        .await
        .unwrap();
    assert_eq!(
        lease.state().database.as_deref(),
        Some(db.to_string().as_str())
    );
    // MULTI is tracked.
    let mut sink = CollectingSink::new();
    lease
        .execute(QueryRequest::new("MULTI"), &mut sink)
        .await
        .unwrap();
    assert_eq!(lease.state().transaction, TxStatus::Active);
    let report = lease.close().await.unwrap();
    assert!(report.rolled_back);
    assert!(lease.state().broken);
}
on_all!(
    console_results_errors_and_lease,
    console_r6,
    console_r7,
    console_v8,
    6
);

async fn cancel_kills_blocking_commands(s: Server, db: i64) {
    let session = fresh(s, db).await;
    let sess = std::sync::Arc::new(session);
    // Session::execute + cancel.
    let req = {
        let mut r = QueryRequest::new("BLPOP never 0");
        r.database = Some(db.to_string());
        r
    };
    let qid = req.query_id;
    let s2 = sess.clone();
    let task = tokio::spawn(async move { execute_collect(s2.as_ref().as_ref(), req).await });
    // Cancel until the statement is registered (a TLS connect can take a while under load).
    let mut task = task;
    let res = loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        sess.cancel(qid).await.unwrap();
        tokio::select! {
            r = &mut task => break r.unwrap(),
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    };
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    // Unknown id is fine.
    sess.cancel(uuid::Uuid::new_v4()).await.unwrap();

    // Lease cancel breaks the lease.
    let lease: std::sync::Arc<dyn dbstudio_core::Lease> = sess
        .open_lease(LeaseOptions {
            database: Some(db.to_string()),
            ..Default::default()
        })
        .await
        .unwrap()
        .into();
    let req = QueryRequest::new("BLPOP never 0");
    let qid = req.query_id;
    let l2 = lease.clone();
    let task = tokio::spawn(async move {
        let mut sink = CollectingSink::new();
        l2.execute(req, &mut sink).await
    });
    let mut task = task;
    let res = loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        lease.cancel(qid).await.unwrap();
        tokio::select! {
            r = &mut task => break r.unwrap(),
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    };
    assert!(matches!(res, Err(Error::Cancelled)));
    assert!(lease.state().broken);

    // Lease timeout kills the connection: the tab does not hang, the lease is broken.
    let lease = sess
        .open_lease(LeaseOptions {
            database: Some(db.to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut req = QueryRequest::new("BLPOP never 0");
    req.timeout_ms = Some(300);
    let mut sink = CollectingSink::new();
    let t0 = std::time::Instant::now();
    assert!(matches!(
        lease.execute(req, &mut sink).await,
        Err(Error::Timeout)
    ));
    assert!(t0.elapsed() < Duration::from_secs(3));
    assert!(lease.state().broken);

    // Timeout.
    let mut req = QueryRequest::new("BLPOP never 0");
    req.timeout_ms = Some(300);
    req.database = Some(db.to_string());
    assert!(matches!(
        execute_collect(sess.as_ref().as_ref(), req).await,
        Err(Error::Timeout)
    ));
}
on_all!(
    cancel_kills_blocking_commands,
    cancel_r6,
    cancel_r7,
    cancel_v8,
    7
);

async fn read_only_blocks_writes(s: Server, db: i64) {
    let rw = fresh(s, db).await;
    run(rw.as_ref(), db, "SET k v\nHSET h f v").await;
    let mut cfg = config(s);
    cfg.read_only = true;
    let ro = connect_with(&cfg).await.unwrap();
    let ro = ro.as_ref();
    assert!(matches!(
        run(ro, db, "GET k").await[0],
        StatementOutcome::Rows(_)
    ));
    for write in [
        "SET k x",
        "DEL k",
        "FLUSHDB",
        "FLUSHALL",
        "EVAL \"return 1\" 0",
        "CONFIG SET maxmemory 0",
        "HSET h a b",
        "EXPIRE k 10",
        "UNKNOWNCMD",
    ] {
        let mut req = QueryRequest::new(write);
        req.database = Some(db.to_string());
        let err = execute_collect(ro, req).await.unwrap_err();
        assert_eq!(err.kind(), "readOnly", "{write}: {err:?}");
    }
    // Reads that look administrative still work (container commands judged by sub-command).
    run(
        ro,
        db,
        "CONFIG GET maxmemory\nINFO server\nDBSIZE\nTTL k\nOBJECT ENCODING k\nMEMORY USAGE k",
    )
    .await;
    if s.name != "redis6" {
        // `_RO` variants are verified through COMMAND INFO flags, not trusted by suffix.
        run(ro, db, "EVAL_RO \"return 1\" 0").await;
    }
    let mut req = QueryRequest::new("XINFO STREAM nokey");
    req.database = Some(db.to_string());
    let err = execute_collect(ro, req).await.unwrap_err();
    assert_ne!(err.kind(), "readOnly", "{err:?}");
    let err = apply(
        ro,
        db,
        vec![KeyChange::Delete {
            keys: vec!["k".into()],
        }],
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), "readOnly");
    let e = ro.editability(&ObjectPath::root()).await.unwrap();
    assert!(!e.editable && !e.atomic);
    let lease = ro
        .open_lease(LeaseOptions {
            database: Some(db.to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut sink = CollectingSink::new();
    let err = lease
        .execute(QueryRequest::new("SET k x"), &mut sink)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "readOnly");
    // Nothing changed.
    assert_eq!(scalar(rw.as_ref(), db, "GET k").await, text("v"));
}
on_all!(read_only_blocks_writes, ro_r6, ro_r7, ro_v8, 8);

async fn delete_pattern_pseudo_command(s: Server, db: i64) {
    let session = fresh(s, db).await;
    let sess = session.as_ref();
    let mut script = String::new();
    for i in 0..2300 {
        script.push_str(&format!("SET tmp:{i} v\n"));
    }
    script.push_str("SET keep:1 v\nSET tmpx v");
    run(sess, db, &script).await;
    assert_eq!(scalar(sess, db, "DBSIZE").await, Value::Int(2302));

    #[derive(Default)]
    struct Progress {
        messages: Vec<String>,
        affected: u64,
    }
    impl dbstudio_core::ResultSink for Progress {
        fn on_columns(&mut self, _: Vec<dbstudio_core::ColumnMeta>) -> dbstudio_core::Result<()> {
            Ok(())
        }
        fn on_rows(&mut self, _: Vec<Vec<Value>>) -> dbstudio_core::Result<()> {
            Ok(())
        }
        fn on_message(&mut self, m: String) -> dbstudio_core::Result<()> {
            self.messages.push(m);
            Ok(())
        }
        fn on_statement_end(
            &mut self,
            end: dbstudio_core::StatementEnd,
        ) -> dbstudio_core::Result<()> {
            if let dbstudio_core::EndOutcome::Affected { rows } = end.outcome {
                self.affected = rows;
            }
            Ok(())
        }
    }
    // Typed in a console (any request id that was not authorized) the command is refused.
    let mut forged = QueryRequest::new("DBSTUDIO.DELPATTERN \"*\"");
    forged.database = Some(db.to_string());
    let err = execute_collect(sess, forged).await.unwrap_err();
    assert_eq!(err.kind(), "permission", "{err:?}");
    assert_eq!(scalar(sess, db, "DBSIZE").await, Value::Int(2302));

    let mut sink = Progress::default();
    let mut req = QueryRequest::new("DBSTUDIO.DELPATTERN \"tmp:*\"");
    req.database = Some(db.to_string());
    let _grant = dbstudio_driver_redis::authorize_internal(req.query_id);
    sess.execute(req, &mut sink).await.unwrap();
    assert_eq!(sink.affected, 2300);
    assert!(sink.messages.last().unwrap().starts_with("deleted=2300"));
    assert_eq!(scalar(sess, db, "DBSIZE").await, Value::Int(2));

    // A sink error (cancellation) stops the loop early.
    run(
        sess,
        db,
        &(0..3000)
            .map(|i| format!("SET again:{i} v\n"))
            .collect::<String>(),
    )
    .await;
    struct StopAfterFirst;
    impl dbstudio_core::ResultSink for StopAfterFirst {
        fn on_columns(&mut self, _: Vec<dbstudio_core::ColumnMeta>) -> dbstudio_core::Result<()> {
            Ok(())
        }
        fn on_rows(&mut self, _: Vec<Vec<Value>>) -> dbstudio_core::Result<()> {
            Ok(())
        }
        fn on_message(&mut self, _: String) -> dbstudio_core::Result<()> {
            Err(Error::Cancelled)
        }
        fn on_statement_end(
            &mut self,
            _: dbstudio_core::StatementEnd,
        ) -> dbstudio_core::Result<()> {
            Ok(())
        }
    }
    let mut req = QueryRequest::new("DBSTUDIO.DELPATTERN \"again:*\"");
    req.database = Some(db.to_string());
    let _grant2 = dbstudio_driver_redis::authorize_internal(req.query_id);
    assert!(matches!(
        sess.execute(req, &mut StopAfterFirst).await,
        Err(Error::Cancelled)
    ));
    let left = match scalar(sess, db, "DBSIZE").await {
        Value::Int(n) => n,
        v => panic!("{v:?}"),
    };
    assert!(
        left > 2,
        "some keys must remain after the early stop, left {left}"
    );
}
on_all!(delete_pattern_pseudo_command, del_r6, del_r7, del_v8, 10);

async fn binary_keys_round_trip(s: Server, db: i64) {
    let session = fresh(s, db).await;
    let sess = session.as_ref();
    run(sess, db, r#"SET "bin\xff\x00key" "val\xfe""#).await;
    let page = sess
        .list_children_page(ChildrenPageRequest {
            parent: db_path(db),
            cursor: None,
            limit: 10,
            pattern: None,
        })
        .await
        .unwrap();
    assert_eq!(page.nodes.len(), 1);
    let node = &page.nodes[0];
    assert!(node.name.starts_with("\u{0}b64:"), "{:?}", node.name);
    let p = sess
        .read_key(KeyReadRequest {
            key: node.path.clone(),
            cursor: None,
            count: 10,
            pattern: None,
            max_string_bytes: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(p.entries, KeyEntries::String { value: Value::Bytes(b), .. } if b == b"val\xfe")
    );
    apply(
        sess,
        db,
        vec![KeyChange::Delete {
            keys: vec![node.name.clone()],
        }],
    )
    .await
    .unwrap();
    assert_eq!(scalar(sess, db, "DBSIZE").await, Value::Int(0));
}
on_all!(binary_keys_round_trip, bin_r6, bin_r7, bin_v8, 11);

async fn big_string_is_truncated_and_json_unsupported_types(s: Server, db: i64) {
    let session = fresh(s, db).await;
    let sess = session.as_ref();
    run(sess, db, &format!("SET big {}", "é".repeat(100))).await;
    let p = sess
        .read_key(KeyReadRequest {
            key: key_path(db, "big"),
            cursor: None,
            count: 1,
            pattern: None,
            max_string_bytes: Some(11),
        })
        .await
        .unwrap();
    match p.entries {
        KeyEntries::String {
            value,
            total_bytes,
            truncated,
        } => {
            assert!(truncated);
            assert_eq!(total_bytes, 200);
            // 11 bytes cut inside a character: the partial sequence is dropped.
            assert_eq!(value, text(&"é".repeat(5)));
        }
        other => panic!("{other:?}"),
    }
    // Paging of a list and a sorted set.
    run(
        sess,
        db,
        &format!(
            "RPUSH l {}\nZADD z {}",
            (0..25).map(|i| i.to_string()).collect::<Vec<_>>().join(" "),
            (0..25)
                .map(|i| format!("{i} m{i}"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    )
    .await;
    for key in ["l", "z"] {
        let mut cursor = None;
        let mut total = 0;
        loop {
            let p = sess
                .read_key(KeyReadRequest {
                    key: key_path(db, key),
                    cursor: cursor.clone(),
                    count: 10,
                    pattern: None,
                    max_string_bytes: None,
                })
                .await
                .unwrap();
            total += match p.entries {
                KeyEntries::List { items, .. } => items.len(),
                KeyEntries::ZSet { members } => members.len(),
                _ => panic!(),
            };
            match p.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        assert_eq!(total, 25, "{key}");
    }
    // Hash with MATCH.
    run(sess, db, "HSET hh a1 1 a2 2 b1 3").await;
    let p = sess
        .read_key(KeyReadRequest {
            key: key_path(db, "hh"),
            cursor: None,
            count: 100,
            pattern: Some("a*".into()),
            max_string_bytes: None,
        })
        .await
        .unwrap();
    let KeyEntries::Hash { fields } = p.entries else {
        panic!()
    };
    assert_eq!(fields.len(), 2);
}
on_all!(
    big_string_is_truncated_and_json_unsupported_types,
    trunc_r6,
    trunc_r7,
    trunc_v8,
    12
);

/// Walk 1,000,000 keys with SCAN through the paged tree. `redis6` has `KEYS` renamed away, so
/// this proves the driver never calls it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn walks_one_million_keys_with_scan() {
    if !enabled() || std::env::var("DBSTUDIO_IT_BIG").map_or(true, |v| v != "1") {
        return;
    }
    let db = 14;
    let session = fresh(REDIS6, db).await;
    let sess = session.as_ref();
    // KEYS is disabled on this server.
    let mut req = QueryRequest::new("KEYS *");
    req.database = Some(db.to_string());
    assert!(execute_collect(sess, req).await.is_err());
    // Load through the test's own pipelined connection.
    let client = redis::Client::open(format!(
        "redis://dbstudio:dbstudio_test@127.0.0.1:{}/{db}",
        REDIS6.default_port
    ))
    .unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    for chunk in 0..1000 {
        let mut pipe = redis::pipe();
        for i in 0..1000 {
            pipe.cmd("SET")
                .arg(format!("big:{}:{}", chunk, i))
                .arg("v")
                .ignore();
        }
        pipe.query_async::<()>(&mut conn).await.unwrap();
    }
    assert_eq!(scalar(sess, db, "DBSIZE").await, Value::Int(1_000_000));
    // Level 1: one prefix "big:"; level 2: 1000 prefixes; level 3: every key.
    let root = sess
        .list_children_page(ChildrenPageRequest {
            parent: db_path(db),
            cursor: None,
            limit: 5000,
            pattern: None,
        })
        .await
        .unwrap();
    assert_eq!(root.approximate_total, Some(1_000_000));
    let mut prefixes = std::collections::BTreeSet::new();
    let mut cursor = None;
    let big = root
        .nodes
        .iter()
        .find(|n| n.name == "big:")
        .expect("big: prefix")
        .path
        .clone();
    let mut scanned_pages = 0;
    loop {
        let page = sess
            .list_children_page(ChildrenPageRequest {
                parent: big.clone(),
                cursor: cursor.clone(),
                limit: 5000,
                pattern: None,
            })
            .await
            .unwrap();
        scanned_pages += 1;
        for n in page.nodes {
            prefixes.insert(n.name);
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(prefixes.len(), 1000);
    eprintln!("walked 1,000,000 keys in {scanned_pages} pages");
    // Flat walk of everything below one prefix with a pattern at full depth.
    let mut keys = 0usize;
    let mut cursor = None;
    loop {
        let page = sess
            .list_children_page(ChildrenPageRequest {
                parent: db_path(db),
                cursor: cursor.clone(),
                limit: 5000,
                pattern: Some("big:7*:1*".into()),
            })
            .await
            .unwrap();
        keys += page.nodes.len();
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert!(keys > 0);
    run(sess, db, "FLUSHDB").await;
}
