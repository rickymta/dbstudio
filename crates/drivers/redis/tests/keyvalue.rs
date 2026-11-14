//! Package K integration tests (`scan_keys`, `KeyChange::Put`, list conflicts, TLS name through
//! SSH) against `redis6`, `redis7`, `valkey8` and `sshd`. They only run when `DBSTUDIO_IT=1`.
//! `DBSTUDIO_IT_BIG=1` adds the 1,000,000 key `scan_keys` walk. Kept in its own binary so the
//! databases it flushes never overlap with `integration.rs`.

#![allow(dead_code, unused_imports)]

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

// -- package K: scan_keys, Put, list conflicts, TLS name through SSH -------------------------

async fn scan_all(
    s: &dyn Session,
    db: i64,
    pattern: Option<&str>,
    type_filter: Option<&str>,
    count: u32,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = None;
    loop {
        let page = s
            .scan_keys(dbstudio_core::KeyScanRequest {
                database: db_path(db),
                pattern: pattern.map(str::to_string),
                cursor: cursor.clone(),
                count,
                type_filter: type_filter.map(str::to_string),
            })
            .await
            .unwrap();
        out.extend(page.keys);
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    out.sort();
    out.dedup();
    out
}

async fn scan_keys_pages_filters_and_binary(s: Server, db: i64) {
    let session = fresh(s, db).await;
    let sess = session.as_ref();
    let mut script = String::new();
    for i in 0..40 {
        script.push_str(&format!("SET a:{i} v\n"));
    }
    script.push_str("HSET h:1 f v\nHSET h:2 f v\nRPUSH l:1 x\n");
    run(sess, db, &script).await;
    let binary = dbstudio_core::encode_key_name(&[0xff, 0x00, b'k']);
    assert!(binary.starts_with(dbstudio_core::BINARY_KEY_PREFIX));
    apply(
        sess,
        db,
        vec![KeyChange::SetString {
            key: binary.clone(),
            value: text("bin"),
            ttl_ms: None,
        }],
    )
    .await
    .unwrap();

    let all = scan_all(sess, db, None, None, 7).await;
    assert_eq!(all.len(), 40 + 3 + 1);
    assert!(all.contains(&binary));
    assert!(all.contains(&"a:17".to_string()));
    // Pattern and type filter.
    assert_eq!(scan_all(sess, db, Some("a:1*"), None, 5).await.len(), 11);
    assert_eq!(
        scan_all(sess, db, None, Some("hash"), 100).await,
        vec!["h:1".to_string(), "h:2".to_string()]
    );
    // A page is bounded by COUNT as a hint: a small count needs several pages.
    let first = sess
        .scan_keys(dbstudio_core::KeyScanRequest {
            database: db_path(db),
            pattern: None,
            cursor: None,
            count: 5,
            type_filter: None,
        })
        .await
        .unwrap();
    assert!(first.next_cursor.is_some());
    assert!(first.keys.len() < 44);
}
on_all!(
    scan_keys_pages_filters_and_binary,
    scankeys_r6,
    scankeys_r7,
    scankeys_v8,
    6
);

/// Everything a key holds, read back through `read_key`.
async fn dump(s: &dyn Session, db: i64, key: &str) -> (String, Vec<String>, Option<i64>) {
    let p = s
        .read_key(KeyReadRequest {
            key: key_path(db, key),
            cursor: None,
            count: 10_000,
            pattern: None,
            max_string_bytes: Some(1 << 20),
        })
        .await
        .unwrap();
    let lines = match &p.entries {
        KeyEntries::String { value, .. } => vec![format!("{value:?}")],
        KeyEntries::Hash { fields } => {
            let mut v: Vec<_> = fields.iter().map(|f| format!("{f:?}")).collect();
            v.sort();
            v
        }
        KeyEntries::List { items, .. } => items.iter().map(|i| format!("{i:?}")).collect(),
        KeyEntries::Set { members } => {
            let mut v: Vec<_> = members.iter().map(|i| format!("{i:?}")).collect();
            v.sort();
            v
        }
        KeyEntries::ZSet { members } => members.iter().map(|m| format!("{m:?}")).collect(),
        KeyEntries::Stream { entries, .. } => entries.iter().map(|e| format!("{e:?}")).collect(),
        other => panic!("{other:?}"),
    };
    (p.key_type, lines, p.ttl_ms)
}

fn put(key: &str, data: dbstudio_core::KeyData, ttl_ms: Option<u64>) -> KeyChange {
    KeyChange::Put {
        key: key.into(),
        data,
        ttl_ms,
        mode: dbstudio_core::PutMode::Replace,
    }
}

async fn put_writes_every_type_replace_and_if_absent(s: Server, db: i64) {
    use dbstudio_core::{KeyData, PutMode, StreamEntry};
    let session = fresh(s, db).await;
    let sess = session.as_ref();
    let binary = dbstudio_core::encode_key_name(&[0xfe, 0x00, 0x01]);
    let r = apply(
        sess,
        db,
        vec![
            put(
                "p:str",
                KeyData::String {
                    value: Value::Bytes(vec![0, 255, 7]),
                },
                Some(60_000),
            ),
            put(
                "p:hash",
                KeyData::Hash {
                    fields: vec![(text("a"), text("1")), (text("b"), text("2"))],
                },
                None,
            ),
            put(
                "p:list",
                KeyData::List {
                    items: vec![text("x"), text("y"), text("x")],
                },
                None,
            ),
            put(
                "p:set",
                KeyData::Set {
                    members: vec![text("m1"), text("m2")],
                },
                None,
            ),
            put(
                "p:zset",
                KeyData::ZSet {
                    members: vec![(text("one"), "1".into()), (text("two"), "2.5".into())],
                },
                None,
            ),
            put(
                "p:stream",
                KeyData::Stream {
                    entries: vec![
                        StreamEntry {
                            id: "1-1".into(),
                            fields: vec![(text("f"), text("v"))],
                        },
                        StreamEntry {
                            id: "2-1".into(),
                            fields: vec![(text("g"), text("w"))],
                        },
                    ],
                },
                None,
            ),
            put(
                &binary,
                KeyData::String {
                    value: text("bin key"),
                },
                None,
            ),
        ],
    )
    .await
    .unwrap();
    assert_eq!(r.applied, 7);
    assert_eq!(r.affected, vec![1; 7]);
    let (t, v, ttl) = dump(sess, db, "p:str").await;
    assert_eq!(
        (t.as_str(), v),
        (
            "string",
            vec![format!("{:?}", Value::Bytes(vec![0, 255, 7]))]
        )
    );
    assert!(
        ttl.is_some_and(|ms| (50_000..=60_000).contains(&ms)),
        "{ttl:?}"
    );
    assert_eq!(dump(sess, db, "p:hash").await.1.len(), 2);
    let (t, v, ttl) = dump(sess, db, "p:list").await;
    assert_eq!((t.as_str(), v.len(), ttl), ("list", 3, None));
    assert_eq!(dump(sess, db, "p:set").await.0, "set");
    assert_eq!(dump(sess, db, "p:zset").await.1.len(), 2);
    let (t, v, _) = dump(sess, db, "p:stream").await;
    assert_eq!((t.as_str(), v.len()), ("stream", 2));
    assert_eq!(
        dump(sess, db, &binary).await.1,
        vec![format!("{:?}", text("bin key"))]
    );
    // No temporary key is left behind.
    assert_eq!(scalar(sess, db, "DBSIZE").await, Value::Int(7));

    // Replace overwrites a key of another type and clears its TTL.
    apply(
        sess,
        db,
        vec![put(
            "p:str",
            KeyData::List {
                items: vec![text("only")],
            },
            None,
        )],
    )
    .await
    .unwrap();
    let (t, v, ttl) = dump(sess, db, "p:str").await;
    assert_eq!((t.as_str(), v.len(), ttl), ("list", 1, None));

    // IfAbsent keeps the existing key and reports 0 affected.
    let r = apply(
        sess,
        db,
        vec![
            KeyChange::Put {
                key: "p:hash".into(),
                data: KeyData::String { value: text("no") },
                ttl_ms: None,
                mode: PutMode::IfAbsent,
            },
            KeyChange::Put {
                key: "p:new".into(),
                data: KeyData::String { value: text("yes") },
                ttl_ms: None,
                mode: PutMode::IfAbsent,
            },
        ],
    )
    .await
    .unwrap();
    assert_eq!(r.affected, vec![0, 1]);
    assert_eq!(dump(sess, db, "p:hash").await.0, "hash");
    assert_eq!(dump(sess, db, "p:new").await.0, "string");
    assert_eq!(scalar(sess, db, "DBSIZE").await, Value::Int(8));

    // An empty collection cannot exist in Redis: invalid input, nothing applied.
    let e = apply(
        sess,
        db,
        vec![put("p:empty", KeyData::List { items: vec![] }, None)],
    )
    .await
    .unwrap_err();
    assert!(matches!(e, Error::ChangeFailed { applied: 0, .. }), "{e:?}");
    // Preview shows the temp key and the final RENAME.
    let lines = sess
        .preview_changes(&ChangeSet::Keys(KeyChangeSet {
            database: db_path(db),
            changes: vec![put("p:x", KeyData::String { value: text("v") }, None)],
        }))
        .await
        .unwrap();
    assert!(lines[0].contains("RENAME <temp> p:x"), "{lines:?}");
}
on_all!(
    put_writes_every_type_replace_and_if_absent,
    put_r6,
    put_r7,
    put_v8,
    7
);

async fn put_big_value_in_chunks(s: Server, db: i64) {
    use dbstudio_core::KeyData;
    let session = fresh(s, db).await;
    let sess = session.as_ref();
    // 50 MB string (written as 7 chunks) and a hash big enough to need several batches.
    let big = vec![b'z'; 50 * 1024 * 1024];
    let started = std::time::Instant::now();
    apply(
        sess,
        db,
        vec![
            put(
                "big:str",
                KeyData::String {
                    value: Value::Bytes(big),
                },
                None,
            ),
            put(
                "big:hash",
                KeyData::Hash {
                    fields: (0..5000)
                        .map(|i| (text(&format!("f{i}")), text(&"x".repeat(200))))
                        .collect(),
                },
                Some(120_000),
            ),
        ],
    )
    .await
    .unwrap();
    eprintln!("put of a 50 MB key took {:?}", started.elapsed());
    assert_eq!(
        scalar(sess, db, "STRLEN big:str").await,
        Value::Int(50 * 1024 * 1024)
    );
    assert_eq!(scalar(sess, db, "HLEN big:hash").await, Value::Int(5000));
    assert_eq!(scalar(sess, db, "DBSIZE").await, Value::Int(2));
    // RENAME is the only step visible to readers: the final key never exists half-written.
    // (Checked indirectly: the temporary name is gone and the length is complete.)
    run(sess, db, "DEL big:str").await;
}
on_all!(put_big_value_in_chunks, putbig_r6, putbig_r7, putbig_v8, 8);

async fn list_conflicts_are_detected(s: Server, db: i64) {
    let session = fresh(s, db).await;
    let sess = session.as_ref();
    run(sess, db, "RPUSH l a b c").await;
    let other = connect(s).await;
    // Another client changes item 1 after this client read "b".
    run(other.as_ref(), db, "LSET l 1 changed-by-other").await;
    let e = apply(
        sess,
        db,
        vec![KeyChange::ListSet {
            key: "l".into(),
            index: 1,
            value: text("mine"),
            expected: Some(text("b")),
        }],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&e, Error::ChangeFailed { message, applied: 0, .. } if message == "list item changed"),
        "{e:?}"
    );
    let e = apply(
        sess,
        db,
        vec![KeyChange::ListRemoveAt {
            key: "l".into(),
            index: 1,
            expected: Some(text("b")),
        }],
    )
    .await
    .unwrap_err();
    assert!(matches!(&e, Error::ChangeFailed { message, .. } if message == "list item changed"));
    // The other client's data is untouched.
    let (_, items, _) = dump(sess, db, "l").await;
    assert_eq!(items.len(), 3);
    assert!(items[1].contains("changed-by-other"), "{items:?}");
    // With the value that is really there, both apply.
    apply(
        sess,
        db,
        vec![KeyChange::ListSet {
            key: "l".into(),
            index: 1,
            value: text("mine"),
            expected: Some(text("changed-by-other")),
        }],
    )
    .await
    .unwrap();
    apply(
        sess,
        db,
        vec![KeyChange::ListRemoveAt {
            key: "l".into(),
            index: 0,
            expected: Some(text("a")),
        }],
    )
    .await
    .unwrap();
    let (_, items, _) = dump(sess, db, "l").await;
    assert_eq!(items.len(), 2);
    assert!(items[0].contains("mine"), "{items:?}");
    // Out of range index counts as a changed list; `expected: None` keeps the old behaviour.
    let e = apply(
        sess,
        db,
        vec![KeyChange::ListSet {
            key: "l".into(),
            index: 9,
            value: text("x"),
            expected: Some(text("c")),
        }],
    )
    .await
    .unwrap_err();
    assert!(matches!(&e, Error::ChangeFailed { message, .. } if message == "list item changed"));
    apply(
        sess,
        db,
        vec![KeyChange::ListSet {
            key: "l".into(),
            index: 0,
            value: text("blind"),
            expected: None,
        }],
    )
    .await
    .unwrap();
    // Preview shows the condition.
    let lines = sess
        .preview_changes(&ChangeSet::Keys(KeyChangeSet {
            database: db_path(db),
            changes: vec![KeyChange::ListSet {
                key: "l".into(),
                index: 0,
                value: text("v"),
                expected: Some(text("blind")),
            }],
        }))
        .await
        .unwrap();
    assert!(lines[0].contains("LINDEX l 0 = blind"), "{lines:?}");
}
on_all!(
    list_conflicts_are_detected,
    listconf_r6,
    listconf_r7,
    listconf_v8,
    9
);

/// Open a tunnel through `sshd` to `redis7:6379`; the first attempt reports the host key, which
/// the test then trusts (like the connection dialog does).
async fn tunnel_to_redis7() -> Option<dbstudio_ssh::Tunnel> {
    use dbstudio_core::{SshAuth, SshConfig};
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
            std::env::var("DBSTUDIO_IT_STRICT").map_or(true, |v| v != "1"),
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
    let target = ("redis7".to_string(), 6379u16);
    let err = dbstudio_ssh::Tunnel::open(&cfg, Some(&secret), target.clone(), &known)
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
        dbstudio_ssh::Tunnel::open(&cfg, Some(&secret), target, &known)
            .await
            .unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tls_name_is_checked_against_the_original_host_through_ssh() {
    if !enabled() {
        return;
    }
    let Some(tunnel) = tunnel_to_redis7().await else {
        return;
    };
    let ca = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../docker/certs/ca.crt");
    // What the application does behind an SSH tunnel: connect to the tunnel end and keep the
    // original host as `tls.server_name`.
    let mut cfg = config(REDIS7);
    cfg.port = tunnel.local_addr().port();
    cfg.tls.mode = TlsMode::VerifyFull;
    cfg.tls.ca_cert_path = Some(ca.into());
    cfg.tls.server_name = Some("redis7".into());
    let session = connect_with(&cfg).await.expect("verifyFull through ssh");
    session.ping().await.unwrap();
    run(session.as_ref(), 10, "SET r6 ok").await;
    assert_eq!(scalar(session.as_ref(), 10, "GET r6").await, text("ok"));
    // Other databases (derived clients) keep the name and the resolver.
    run(session.as_ref(), 11, "SET r6 ok2").await;
    // A name that is not in the certificate must be refused (127.0.0.1 itself IS in its SAN, so
    // only the original-host check can reject this).
    cfg.tls.server_name = Some("not-in-the-cert.example".into());
    let err = connect_with(&cfg).await.err().expect("must fail");
    assert!(matches!(err, Error::Tls(_)), "{err:?}");
}

/// `scan_keys` over 1,000,000 keys (`redis6` has `KEYS` renamed away) in pages of 10,000.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scan_keys_walks_one_million_keys() {
    if !enabled() || std::env::var("DBSTUDIO_IT_BIG").map_or(true, |v| v != "1") {
        return;
    }
    let db = 13;
    let session = fresh(REDIS6, db).await;
    let sess = session.as_ref();
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
                .arg(format!("big:{chunk}:{i}"))
                .arg("v")
                .ignore();
        }
        pipe.query_async::<()>(&mut conn).await.unwrap();
    }
    let started = std::time::Instant::now();
    let keys = scan_all(sess, db, None, None, 10_000).await;
    eprintln!(
        "scan_keys walked {} keys in {:?}",
        keys.len(),
        started.elapsed()
    );
    assert_eq!(keys.len(), 1_000_000);
    run(sess, db, "FLUSHDB").await;
}
