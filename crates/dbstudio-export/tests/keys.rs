//! Redis keys export / import against an in-memory key-value session (no server needed).

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::Recorder;
use dbstudio_core::{
    ApplyResult, ChangeSet, DbKind, Error, ExecSummary, JobReporter, KeyChange, KeyData,
    KeyEntries, KeyReadRequest, KeyRecord, KeyScanPage, KeyScanRequest, KeyValuePage, ObjectKind,
    ObjectNode, ObjectPath, PutMode, QueryRequest, Result, ResultSink, ScriptDialect, ServerInfo,
    Session, StreamEntry, Value, encode_key_name,
};
use dbstudio_export::{
    CsvOptions, ExportContent, ExportContext, ExportDestination, ExportFormat, ExportSource,
    ExportSpec, ImportFormat, ImportMode, ImportSpec, ImportTarget, JsonLayout, JsonOptions,
    ValueFormat, run_export, run_import,
};
use uuid::Uuid;

/// One stored key.
#[derive(Clone)]
struct Stored {
    data: KeyData,
    ttl_ms: Option<i64>,
}

/// Key-value session: `SCAN` pages of 3 keys, `read_key` pages of 2 entries.
#[derive(Default)]
struct KvSession {
    keys: Mutex<BTreeMap<String, Stored>>,
    /// Keys that vanish when read.
    vanish: Mutex<Vec<String>>,
    /// An `apply_changes` call with more than this many changes fails.
    fail_batches_over: Mutex<Option<usize>>,
    /// A `Put` for this key always fails.
    fail_key: Mutex<Option<String>>,
}

impl KvSession {
    fn with(keys: Vec<(&str, KeyData, Option<i64>)>) -> Arc<KvSession> {
        let s = KvSession::default();
        for (k, data, ttl_ms) in keys {
            s.keys
                .lock()
                .unwrap()
                .insert(k.to_string(), Stored { data, ttl_ms });
        }
        Arc::new(s)
    }
}

fn page_of<T: Clone>(items: &[T], cursor: Option<&str>, size: usize) -> (Vec<T>, Option<String>) {
    let start: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
    let end = (start + size).min(items.len());
    (
        items[start..end].to_vec(),
        (end < items.len()).then(|| end.to_string()),
    )
}

#[async_trait]
impl Session for KvSession {
    async fn ping(&self) -> Result<()> {
        Ok(())
    }
    async fn server_info(&self) -> Result<ServerInfo> {
        Ok(ServerInfo::default())
    }
    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        assert_eq!(req.text, "DBSIZE");
        sink.on_columns(vec![])?;
        sink.on_rows(vec![vec![Value::Int(
            self.keys.lock().unwrap().len() as i64
        )]])?;
        Ok(ExecSummary::default())
    }
    async fn list_children(&self, _p: &ObjectPath) -> Result<Vec<ObjectNode>> {
        Ok(vec![])
    }
    async fn scan_keys(&self, req: KeyScanRequest) -> Result<KeyScanPage> {
        let names: Vec<String> = self
            .keys
            .lock()
            .unwrap()
            .keys()
            .filter(|k| match req.pattern.as_deref() {
                Some(p) if p.ends_with('*') => k.starts_with(&p[..p.len() - 1]),
                Some(p) => *k == p,
                None => true,
            })
            .cloned()
            .collect();
        let (keys, next_cursor) = page_of(&names, req.cursor.as_deref(), 3);
        Ok(KeyScanPage { keys, next_cursor })
    }
    async fn read_key(&self, req: KeyReadRequest) -> Result<KeyValuePage> {
        let name = req.key.last().unwrap().name.clone();
        if self.vanish.lock().unwrap().contains(&name) {
            return Err(Error::NotFound(format!("key {name}")));
        }
        let stored = self
            .keys
            .lock()
            .unwrap()
            .get(&name)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("key {name}")))?;
        let c = req.cursor.as_deref();
        let (entries, next_cursor, key_type) = match &stored.data {
            KeyData::String { value } => (
                KeyEntries::String {
                    value: value.clone(),
                    total_bytes: 1,
                    truncated: false,
                },
                None,
                "string",
            ),
            KeyData::Hash { fields } => {
                let (f, n) = page_of(fields, c, 2);
                (KeyEntries::Hash { fields: f }, n, "hash")
            }
            KeyData::List { items } => {
                let (i, n) = page_of(items, c, 2);
                (KeyEntries::List { start: 0, items: i }, n, "list")
            }
            KeyData::Set { members } => {
                let (m, n) = page_of(members, c, 2);
                (KeyEntries::Set { members: m }, n, "set")
            }
            KeyData::ZSet { members } => {
                let (m, n) = page_of(members, c, 2);
                (KeyEntries::ZSet { members: m }, n, "zset")
            }
            KeyData::Stream { entries } => {
                let (e, n) = page_of(entries, c, 2);
                (
                    KeyEntries::Stream {
                        entries: e,
                        groups: 1,
                    },
                    n,
                    "stream",
                )
            }
            KeyData::Json { text } => (KeyEntries::Json { text: text.clone() }, None, "json"),
        };
        Ok(KeyValuePage {
            key: req.key,
            key_type: key_type.into(),
            ttl_ms: stored.ttl_ms,
            length: None,
            memory_bytes: None,
            encoding: None,
            entries,
            next_cursor,
        })
    }
    async fn apply_changes(&self, changes: ChangeSet) -> Result<ApplyResult> {
        let ChangeSet::Keys(set) = changes else {
            return Err(Error::Unsupported("data_editing".into()));
        };
        if self
            .fail_batches_over
            .lock()
            .unwrap()
            .is_some_and(|n| set.changes.len() > n)
        {
            return Err(Error::Other("batch refused".into()));
        }
        let mut affected = Vec::new();
        for change in set.changes {
            let KeyChange::Put {
                key,
                data,
                ttl_ms,
                mode,
            } = change
            else {
                return Err(Error::Unsupported("not a put".into()));
            };
            if self.fail_key.lock().unwrap().as_deref() == Some(key.as_str()) {
                return Err(Error::Other(format!("cannot write {key}")));
            }
            let mut keys = self.keys.lock().unwrap();
            if mode == PutMode::IfAbsent && keys.contains_key(&key) {
                affected.push(0);
                continue;
            }
            keys.insert(
                key,
                Stored {
                    data,
                    ttl_ms: ttl_ms.map(|t| t as i64),
                },
            );
            affected.push(1);
        }
        Ok(ApplyResult {
            applied: affected.len() as u32,
            affected,
            atomic: false,
        })
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn sample() -> Vec<(&'static str, KeyData, Option<i64>)> {
    vec![
        (
            "s:plain",
            KeyData::String {
                value: text("hello"),
            },
            None,
        ),
        (
            "s:ttl",
            KeyData::String { value: text("tmp") },
            Some(3_600_000),
        ),
        (
            "h:user",
            KeyData::Hash {
                fields: vec![
                    (text("a"), text("1")),
                    (text("b"), text("2")),
                    (text("c"), text("3")),
                ],
            },
            None,
        ),
        (
            "l:list",
            KeyData::List {
                items: (1..=5).map(|i| text(&format!("i{i}"))).collect(),
            },
            None,
        ),
        (
            "set:s",
            KeyData::Set {
                members: vec![text("x"), text("y"), text("z")],
            },
            None,
        ),
        (
            "z:z",
            KeyData::ZSet {
                members: vec![(text("m1"), "1.5".into()), (text("m2"), "inf".into())],
            },
            None,
        ),
        (
            "x:stream",
            KeyData::Stream {
                entries: vec![
                    StreamEntry {
                        id: "1-0".into(),
                        fields: vec![(text("f"), text("v"))],
                    },
                    StreamEntry {
                        id: "2-0".into(),
                        fields: vec![(text("f"), text("w"))],
                    },
                    StreamEntry {
                        id: "3-0".into(),
                        fields: vec![(text("f"), text("u"))],
                    },
                ],
            },
            None,
        ),
        (
            "j:doc",
            KeyData::Json {
                text: "{\"a\":1}".into(),
            },
            None,
        ),
        (
            // Binary key and value.
            "\0b64:AJ+Slv8=",
            KeyData::String {
                value: Value::Bytes(vec![0, 159, 146, 150, 255]),
            },
            None,
        ),
    ]
}

fn db() -> ObjectPath {
    ObjectPath::root().child(ObjectKind::Database, "0")
}

fn ctx(session: Arc<dyn Session>, reporter: Arc<Recorder>) -> ExportContext {
    ExportContext {
        session,
        kind: DbKind::Redis,
        dialect: ScriptDialect::RedisCli,
        reporter,
    }
}

fn export_spec(dest: &Path, format: ExportFormat, pattern: &str) -> ExportSpec {
    ExportSpec {
        connection_id: Uuid::nil(),
        source: ExportSource::Keys {
            database: db(),
            pattern: pattern.into(),
        },
        content: ExportContent::Data,
        format,
        destination: ExportDestination {
            path: dest.to_string_lossy().into_owned(),
            overwrite: false,
        },
        values: ValueFormat::default(),
    }
}

fn ndjson() -> ExportFormat {
    ExportFormat::Json(JsonOptions {
        layout: JsonLayout::Ndjson,
        ..JsonOptions::default()
    })
}

fn import_spec(file: &Path, layout: JsonLayout, mode: PutMode, max_errors: u32) -> ImportSpec {
    ImportSpec {
        connection_id: Uuid::nil(),
        file: file.to_string_lossy().into_owned(),
        format: ImportFormat::Json { layout },
        target: ImportTarget::Keys {
            database: db(),
            mode,
        },
        mapping: vec![],
        on_conflict: None,
        mode: ImportMode::Batches { rows: 100 },
        max_errors,
    }
}

fn records(path: &str) -> Vec<KeyRecord> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn export_reads_every_type_page_by_page_and_import_restores_them() {
    let dir = tempfile::tempdir().unwrap();
    let source = KvSession::with(sample());
    let report = run_export(
        ctx(source.clone(), Recorder::new()),
        export_spec(&dir.path().join("k.ndjson"), ndjson(), "*"),
    )
    .await
    .unwrap();
    assert_eq!((report.rows, report.objects), (9, 1));
    let recs = records(&report.files[0]);
    assert_eq!(recs.len(), 9);
    let by_key = |k: &str| recs.iter().find(|r| r.key == k).unwrap();
    // Collections read over several pages are whole.
    assert_eq!(
        by_key("l:list").data,
        source.keys.lock().unwrap()["l:list"].data
    );
    assert_eq!(
        by_key("h:user").data,
        source.keys.lock().unwrap()["h:user"].data
    );
    // TTL becomes an absolute expiry.
    assert_eq!(by_key("s:plain").expire_at_ms, None);
    let at = by_key("s:ttl").expire_at_ms.unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!((at - now - 3_600_000).abs() < 5_000, "{at} vs {now}");
    // The binary key survives under its encoded name.
    assert_eq!(
        by_key(&encode_key_name(&[0, 159, 146, 150, 255])).key,
        "\0b64:AJ+Slv8="
    );
    // A stream with consumer groups is exported but the groups are reported.
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("consumer groups"))
    );

    // Import into an empty database.
    let target = KvSession::with(vec![]);
    let imported = run_import(
        ctx(target.clone(), Recorder::new()),
        import_spec(
            Path::new(&report.files[0]),
            JsonLayout::Ndjson,
            PutMode::Replace,
            0,
        ),
    )
    .await
    .unwrap();
    assert_eq!((imported.rows_read, imported.inserted), (9, 9));
    let got = target.keys.lock().unwrap();
    let want = source.keys.lock().unwrap();
    assert_eq!(got.len(), want.len());
    for (k, w) in want.iter() {
        assert_eq!(got[k].data, w.data, "key {k:?}");
    }
    let ttl = got["s:ttl"].ttl_ms.unwrap();
    assert!((ttl - 3_600_000).abs() < 5_000, "ttl {ttl}");
    assert_eq!(got["s:plain"].ttl_ms, None);
}

#[tokio::test]
async fn the_pattern_filters_and_the_array_layout_works() {
    let dir = tempfile::tempdir().unwrap();
    let source = KvSession::with(sample());
    let report = run_export(
        ctx(source, Recorder::new()),
        export_spec(
            &dir.path().join("k.json"),
            ExportFormat::Json(JsonOptions::default()),
            "s:*",
        ),
    )
    .await
    .unwrap();
    assert_eq!(report.rows, 2);
    let doc: Vec<KeyRecord> =
        serde_json::from_str(&std::fs::read_to_string(&report.files[0]).unwrap()).unwrap();
    assert_eq!(doc.len(), 2);
    // And back through the array layout.
    let target = KvSession::with(vec![]);
    let r = run_import(
        ctx(target.clone(), Recorder::new()),
        import_spec(
            Path::new(&report.files[0]),
            JsonLayout::Array,
            PutMode::Replace,
            0,
        ),
    )
    .await
    .unwrap();
    assert_eq!(r.inserted, 2);
    assert_eq!(target.keys.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn keys_that_vanish_are_skipped_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let source = KvSession::with(sample());
    source.vanish.lock().unwrap().push("s:plain".into());
    let report = run_export(
        ctx(source, Recorder::new()),
        export_spec(&dir.path().join("k.ndjson"), ndjson(), "*"),
    )
    .await
    .unwrap();
    assert_eq!(report.rows, 8);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("1 key(s) disappeared")),
        "{:?}",
        report.warnings
    );
}

#[tokio::test]
async fn only_json_is_accepted_for_keys() {
    let dir = tempfile::tempdir().unwrap();
    let source = KvSession::with(sample());
    let err = run_export(
        ctx(source.clone(), Recorder::new()),
        export_spec(
            &dir.path().join("k.csv"),
            ExportFormat::Csv(CsvOptions::default()),
            "*",
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), "invalid", "{err:?}");
    assert!(!dir.path().join("k.csv").exists());
    // Import: not a JSON file.
    let file = dir.path().join("x.csv");
    std::fs::write(&file, "a\n1\n").unwrap();
    let mut spec = import_spec(&file, JsonLayout::Array, PutMode::Replace, 0);
    spec.format = ImportFormat::Csv {
        delimiter: ',',
        quote: '"',
        header: true,
        encoding: dbstudio_export::TextEncoding::Utf8,
        null_text: None,
    };
    let err = run_import(ctx(source, Recorder::new()), spec)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "invalid", "{err:?}");
}

#[tokio::test]
async fn import_skips_expired_keys_and_honours_if_absent() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("in.ndjson");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let rec = |key: &str, v: &str, at: Option<i64>| {
        serde_json::to_string(&KeyRecord {
            key: key.into(),
            data: KeyData::String { value: text(v) },
            expire_at_ms: at,
        })
        .unwrap()
    };
    std::fs::write(
        &file,
        [
            rec("old", "x", Some(now - 10_000)),
            rec("exists", "new", None),
            rec("fresh", "y", Some(now + 60_000)),
        ]
        .join("\n"),
    )
    .unwrap();
    let target = KvSession::with(vec![(
        "exists",
        KeyData::String {
            value: text("keep"),
        },
        None,
    )]);
    let r = run_import(
        ctx(target.clone(), Recorder::new()),
        import_spec(&file, JsonLayout::Ndjson, PutMode::IfAbsent, 0),
    )
    .await
    .unwrap();
    // `old` is expired (skipped), `exists` is kept (skipped), `fresh` is written.
    assert_eq!((r.rows_read, r.inserted, r.skipped), (3, 1, 2));
    let keys = target.keys.lock().unwrap();
    assert!(!keys.contains_key("old"));
    assert_eq!(
        keys["exists"].data,
        KeyData::String {
            value: text("keep")
        }
    );
    assert!(keys.contains_key("fresh"));
}

#[tokio::test]
async fn bad_records_and_failing_keys_are_reported_per_line() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("in.ndjson");
    let rec = |key: &str| {
        serde_json::to_string(&KeyRecord {
            key: key.into(),
            data: KeyData::String { value: text("v") },
            expire_at_ms: None,
        })
        .unwrap()
    };
    std::fs::write(
        &file,
        format!(
            "{}\nnot json at all\n{}\n{}\n",
            rec("a"),
            rec("bad"),
            rec("b")
        ),
    )
    .unwrap();
    let target = KvSession::with(vec![]);
    *target.fail_key.lock().unwrap() = Some("bad".into());
    // The whole batch fails once, then key by key.
    let r = run_import(
        ctx(target.clone(), Recorder::new()),
        import_spec(&file, JsonLayout::Ndjson, PutMode::Replace, 5),
    )
    .await
    .unwrap();
    assert_eq!((r.rows_read, r.inserted), (4, 2));
    let lines: Vec<u64> = r.errors.iter().map(|e| e.line).collect();
    assert_eq!(lines, [2, 3]);
    // Without tolerance the import stops at the first error.
    let err = run_import(
        ctx(KvSession::with(vec![]), Recorder::new()),
        import_spec(&file, JsonLayout::Ndjson, PutMode::Replace, 0),
    )
    .await
    .unwrap_err();
    assert!(err.message().contains("line 2"), "{err:?}");
}

#[tokio::test]
async fn cancelling_stops_the_export_and_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let source = KvSession::with(sample());
    let reporter = Recorder::new();
    reporter.token.cancel();
    let dest = dir.path().join("k.ndjson");
    let err = run_export(
        ctx(source, reporter.clone() as Arc<Recorder>),
        export_spec(&dest, ndjson(), "*"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Cancelled));
    assert!(!dest.exists());
    let _: &dyn JobReporter = &*reporter;
}
