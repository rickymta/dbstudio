//! Redis keys: export (`ExportSource::Keys`) to JSON and import (`ImportTarget::Keys`) from it.
//!
//! The JSON file holds one [`KeyRecord`] per element (array or NDJSON): the key name, the whole
//! value by type and the absolute expiry. Export walks the keyspace with `Session::scan_keys`
//! and reads each key completely with paged `read_key` calls; import writes the records back
//! with `KeyChange::Put` in batches.

use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use dbstudio_core::{
    ApplyResult, CancelToken, ChangeSet, CollectingSink, ColumnMeta, Error, JobProgress, KeyChange,
    KeyChangeSet, KeyData, KeyEntries, KeyReadRequest, KeyRecord, KeyScanRequest, ObjectKind,
    ObjectPath, PutMode, QueryRequest, Result, Session, StatementOutcome, Value, ValueKind,
    decode_key_name,
};

use crate::conn::database_of;
use crate::engine::output_path_for;
use crate::import::source::JsonRecords;
use crate::writers::{WriterConfig, create_writer};
use crate::{
    ExportContent, ExportContext, ExportFormat, ExportReport, ExportSpec, ExportTuning,
    ImportFormat, ImportReport, ImportSpec, ImportTarget, ObjectMeta, RowError,
};

/// Keys requested per `SCAN` page.
const SCAN_COUNT: u32 = 1000;
/// Entries requested per `read_key` page.
const READ_COUNT: u32 = 1000;
/// Largest string exported; a bigger one is skipped with a warning.
pub(crate) const MAX_STRING_BYTES: u64 = 512 * 1024 * 1024;
/// Keys per `apply_changes` call of the import.
const IMPORT_BATCH: usize = 200;
/// Individual warnings kept before the rest is summarised.
const MAX_WARNINGS: usize = 20;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

// ---------------------------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------------------------

/// Why a key is not in the export.
enum Skip {
    /// Deleted (or expired) between `SCAN` and the read.
    Gone,
    /// Skipped, with the reason shown to the user.
    Other(String),
}

/// A key read completely.
struct Whole {
    data: KeyData,
    ttl_ms: Option<i64>,
    /// Consumer groups of a stream (not exported).
    groups: u64,
}

/// Read every page of one key.
async fn read_whole(session: &dyn Session, database: &ObjectPath, name: &str) -> Result<Whole> {
    let key = database.clone().child(ObjectKind::Key, name);
    let mut cursor: Option<String> = None;
    let mut acc: Option<KeyData> = None;
    let (mut ttl_ms, mut groups) = (None, 0u64);
    loop {
        let page = session
            .read_key(KeyReadRequest {
                key: key.clone(),
                cursor: cursor.clone(),
                count: READ_COUNT,
                pattern: None,
                max_string_bytes: Some(MAX_STRING_BYTES),
            })
            .await?;
        if acc.is_none() {
            ttl_ms = page.ttl_ms;
        }
        match (page.entries, &mut acc) {
            (
                KeyEntries::String {
                    truncated: true, ..
                },
                _,
            ) => {
                return Err(Error::Invalid(format!(
                    "string larger than {} MiB",
                    MAX_STRING_BYTES / 1024 / 1024
                )));
            }
            (KeyEntries::String { value, .. }, None) => acc = Some(KeyData::String { value }),
            (KeyEntries::Hash { fields }, None) => acc = Some(KeyData::Hash { fields }),
            (KeyEntries::Hash { fields }, Some(KeyData::Hash { fields: all })) => {
                all.extend(fields);
            }
            (KeyEntries::List { items, .. }, None) => acc = Some(KeyData::List { items }),
            (KeyEntries::List { items, .. }, Some(KeyData::List { items: all })) => {
                all.extend(items);
            }
            (KeyEntries::Set { members }, None) => acc = Some(KeyData::Set { members }),
            (KeyEntries::Set { members }, Some(KeyData::Set { members: all })) => {
                all.extend(members);
            }
            (KeyEntries::ZSet { members }, None) => acc = Some(KeyData::ZSet { members }),
            (KeyEntries::ZSet { members }, Some(KeyData::ZSet { members: all })) => {
                all.extend(members);
            }
            (KeyEntries::Stream { entries, groups: g }, None) => {
                groups = g;
                acc = Some(KeyData::Stream { entries });
            }
            (KeyEntries::Stream { entries, .. }, Some(KeyData::Stream { entries: all })) => {
                all.extend(entries);
            }
            (KeyEntries::Json { text }, None) => acc = Some(KeyData::Json { text }),
            (KeyEntries::Unsupported { type_name }, _) => {
                return Err(Error::Unsupported(format!("key type {type_name}")));
            }
            _ => {
                return Err(Error::Other(
                    "the key changed type while it was read".into(),
                ));
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    let data = acc.ok_or_else(|| Error::Other("empty key page".into()))?;
    Ok(Whole {
        data,
        ttl_ms,
        groups,
    })
}

/// `DBSIZE` of the database, for the progress bar; `None` when it cannot be read.
async fn dbsize(session: &dyn Session, database: Option<String>) -> Option<u64> {
    let mut req = QueryRequest::new("DBSIZE");
    req.database = database;
    let mut sink = CollectingSink::new();
    session.execute(req, &mut sink).await.ok()?;
    sink.into_results()
        .into_iter()
        .find_map(|r| match r.outcome {
            StatementOutcome::Rows(rs) => rs.rows.first().and_then(|row| match row.first() {
                Some(Value::Int(i)) => u64::try_from(*i).ok(),
                Some(Value::UInt(u)) => Some(*u),
                _ => None,
            }),
            _ => None,
        })
}

fn key_columns() -> Vec<ColumnMeta> {
    let col = |name: &str, kind| ColumnMeta {
        name: name.into(),
        type_name: name.into(),
        value_kind: kind,
        nullable: Some(true),
    };
    vec![
        col("key", ValueKind::Text),
        col("data", ValueKind::Json),
        col("expireAtMs", ValueKind::Int),
    ]
}

/// Export the keys of a Redis database as [`KeyRecord`]s.
pub(crate) async fn export_keys(
    ctx: &ExportContext,
    spec: &ExportSpec,
    tuning: &ExportTuning,
) -> Result<ExportReport> {
    let ExportSource::Keys { database, pattern } = &spec.source else {
        return Err(Error::Invalid("not a keys export".into()));
    };
    let ExportFormat::Json(opts) = &spec.format else {
        return Err(Error::Invalid(
            "Redis keys can only be exported as JSON".into(),
        ));
    };
    if spec.content == ExportContent::Schema {
        return Err(Error::Invalid(
            "Redis keys have no structure to export".into(),
        ));
    }
    let _ = opts;
    let cancel = ctx.reporter.cancel_token().clone();
    let mut warnings: Vec<String> = Vec::new();
    let dest = output_path_for(spec, 1, &mut warnings);
    crate::fsio::check_destination(&dest, spec.destination.overwrite)?;

    let mut writer = create_writer(WriterConfig {
        format: spec.format.clone(),
        dialect: ctx.dialect,
        binary: spec.values.binary,
        content: ExportContent::Data,
        dest,
        overwrite: spec.destination.overwrite,
        zip: false,
        tuning: *tuning,
        flatten_fields: Vec::new(),
        kind: ctx.kind,
    })?;
    let session = ctx.session.as_ref();
    let db_name = database_of(database);
    let total = dbsize(session, db_name).await;
    let mut progress = JobProgress {
        phase: "data".into(),
        object: Some("keys".into()),
        objects_total: Some(1),
        rows_total: total,
        ..JobProgress::default()
    };
    ctx.reporter.progress(&progress);
    writer.begin_object(&ObjectMeta {
        path: None,
        name: "keys".into(),
        columns: key_columns(),
        details: None,
        estimated_rows: total,
    })?;

    let (mut rows, mut gone, mut groups_skipped) = (0u64, 0u64, 0u64);
    let mut seen: HashSet<String> = HashSet::new();
    let mut cursor: Option<String> = None;
    let mut skipped_notes = 0usize;
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let page = session
            .scan_keys(KeyScanRequest {
                database: database.clone(),
                pattern: Some(pattern.clone()).filter(|p| !p.is_empty()),
                cursor: cursor.clone(),
                count: SCAN_COUNT,
                type_filter: None,
            })
            .await?;
        let mut batch: Vec<Vec<Value>> = Vec::with_capacity(page.keys.len());
        for name in page.keys {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            // `SCAN` may return a key more than once.
            if !seen.insert(name.clone()) {
                continue;
            }
            match read_one(session, database, &name, &cancel).await? {
                Ok((record, groups)) => {
                    if groups > 0 {
                        groups_skipped += 1;
                    }
                    batch.push(record_row(&record)?);
                }
                Err(Skip::Gone) => gone += 1,
                Err(Skip::Other(reason)) => {
                    skipped_notes += 1;
                    if skipped_notes <= MAX_WARNINGS {
                        warnings.push(format!(
                            "key {} skipped: {reason}",
                            name.chars().take(200).collect::<String>().escape_debug()
                        ));
                    }
                }
            }
        }
        if !batch.is_empty() {
            let n = batch.len() as u64;
            writer.on_rows(batch)?;
            rows += n;
            progress.rows_done = rows;
            progress.bytes_done = writer.bytes();
            ctx.reporter.progress(&progress);
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    writer.end_object()?;
    if skipped_notes > MAX_WARNINGS {
        warnings.push(format!(
            "{} more key(s) were skipped",
            skipped_notes - MAX_WARNINGS
        ));
    }
    if gone > 0 {
        warnings.push(format!(
            "{gone} key(s) disappeared while the export ran and were skipped"
        ));
    }
    if groups_skipped > 0 {
        warnings.push(format!(
            "{groups_skipped} stream(s) have consumer groups, which are not exported"
        ));
    }
    progress.objects_done = 1;
    progress.phase = "finish".into();
    ctx.reporter.progress(&progress);
    warnings.extend(writer.warnings());
    let files = writer.finish()?;
    let bytes = files
        .iter()
        .filter_map(|f| std::fs::metadata(f).ok())
        .map(|m| m.len())
        .sum();
    Ok(ExportReport {
        files: files
            .iter()
            .map(|f| f.to_string_lossy().into_owned())
            .collect(),
        objects: 1,
        rows,
        bytes,
        warnings,
        object_list: Vec::new(),
    })
}

use crate::ExportSource;

/// Read one key into a record. The outer error aborts the export; the inner one skips the key.
async fn read_one(
    session: &dyn Session,
    database: &ObjectPath,
    name: &str,
    cancel: &CancelToken,
) -> Result<std::result::Result<(KeyRecord, u64), Skip>> {
    match read_whole(session, database, name).await {
        Ok(whole) => {
            let expire_at_ms = whole.ttl_ms.filter(|t| *t > 0).map(|t| now_ms() + t);
            Ok(Ok((
                KeyRecord {
                    key: name.to_string(),
                    data: whole.data,
                    expire_at_ms,
                },
                whole.groups,
            )))
        }
        Err(Error::NotFound(_)) => Ok(Err(Skip::Gone)),
        Err(Error::Invalid(m) | Error::Unsupported(m)) => Ok(Err(Skip::Other(m))),
        Err(e) if cancel.is_cancelled() => {
            let _ = e;
            Err(Error::Cancelled)
        }
        Err(e) => Err(e),
    }
}

/// Row of the JSON writer: `key`, `data` as embedded JSON, `expireAtMs`.
fn record_row(record: &KeyRecord) -> Result<Vec<Value>> {
    let data = serde_json::to_string(&record.data)
        .map_err(|e| Error::Other(format!("cannot encode key {}: {e}", record.key)))?;
    Ok(vec![
        Value::Text(record.key.clone()),
        Value::Json(data),
        record.expire_at_ms.map_or(Value::Null, Value::Int),
    ])
}

// ---------------------------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------------------------

/// Import the records of a JSON file as Redis keys.
pub(crate) async fn import_keys(
    ctx: &ExportContext,
    spec: &ImportSpec,
    cancel: &CancelToken,
) -> Result<ImportReport> {
    let ImportTarget::Keys { database, mode } = &spec.target else {
        return Err(Error::Invalid("not a keys import".into()));
    };
    let ImportFormat::Json { layout } = &spec.format else {
        return Err(Error::Invalid(
            "Redis keys can only be imported from a JSON file".into(),
        ));
    };
    let path = Path::new(&spec.file);
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let (open_path, layout) = (path.to_path_buf(), *layout);
    let mut records = tokio::task::spawn_blocking(move || JsonRecords::open(&open_path, layout))
        .await
        .map_err(|e| Error::Other(format!("import task failed: {e}")))??;

    let mut report = ImportReport::default();
    let mut progress = JobProgress {
        phase: "import".into(),
        object: Some("keys".into()),
        bytes_total: Some(size),
        ..JobProgress::default()
    };
    let mut pending: Vec<(u64, KeyChange)> = Vec::new();
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let record = records.next()?;
        let done = record.is_none();
        if let Some((line, text)) = record {
            report.rows_read += 1;
            match prepare(&text, *mode) {
                Ok(Some(change)) => pending.push((line, change)),
                Ok(None) => report.skipped += 1,
                Err(message) => row_failed(&mut report, spec.max_errors, line, message)?,
            }
        }
        if pending.len() >= IMPORT_BATCH || (done && !pending.is_empty()) {
            flush(ctx, database, &mut pending, &mut report, spec.max_errors).await?;
            progress.rows_done = report.rows_read;
            ctx.reporter.progress(&progress);
        }
        if done {
            break;
        }
    }
    Ok(report)
}

/// One record as a `Put`. `Ok(None)`: already expired, not imported.
fn prepare(text: &str, mode: PutMode) -> std::result::Result<Option<KeyChange>, String> {
    let record: KeyRecord =
        serde_json::from_str(text).map_err(|e| format!("not a Redis key record: {e}"))?;
    decode_key_name(&record.key).map_err(|e| e.message())?;
    let ttl_ms = match record.expire_at_ms {
        None => None,
        Some(at) => {
            let left = at - now_ms();
            if left <= 0 {
                return Ok(None);
            }
            Some(left as u64)
        }
    };
    Ok(Some(KeyChange::Put {
        key: record.key,
        data: record.data,
        ttl_ms,
        mode,
    }))
}

fn row_failed(
    report: &mut ImportReport,
    max_errors: u32,
    line: u64,
    message: String,
) -> Result<()> {
    report.errors.push(RowError {
        line,
        message: message.clone(),
    });
    if report.errors.len() as u32 > max_errors {
        return Err(Error::Query {
            message: format!("line {line}: {message}"),
            position: None,
        });
    }
    Ok(())
}

fn count(report: &mut ImportReport, affected: u64) {
    if affected > 0 {
        report.inserted += 1;
    } else {
        // `IfAbsent` met an existing key.
        report.skipped += 1;
    }
}

/// Write the pending keys. A failing batch is retried key by key so that only the bad keys are
/// reported.
async fn flush(
    ctx: &ExportContext,
    database: &ObjectPath,
    pending: &mut Vec<(u64, KeyChange)>,
    report: &mut ImportReport,
    max_errors: u32,
) -> Result<()> {
    let batch = std::mem::take(pending);
    let set = |changes: Vec<KeyChange>| {
        ChangeSet::Keys(KeyChangeSet {
            database: database.clone(),
            changes,
        })
    };
    let whole = ctx
        .session
        .apply_changes(set(batch.iter().map(|(_, c)| c.clone()).collect()))
        .await;
    match whole {
        Ok(ApplyResult { affected, .. }) if affected.len() == batch.len() => {
            for a in affected {
                count(report, a);
            }
            return Ok(());
        }
        Ok(_) => {
            // The driver did not report per key: all were written.
            report.inserted += batch.len() as u64;
            return Ok(());
        }
        Err(Error::Cancelled) => return Err(Error::Cancelled),
        Err(_) => {}
    }
    for (line, change) in batch {
        if ctx.reporter.cancel_token().is_cancelled() {
            return Err(Error::Cancelled);
        }
        match ctx.session.apply_changes(set(vec![change])).await {
            Ok(r) => count(report, r.affected.first().copied().unwrap_or(1)),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => row_failed(report, max_errors, line, e.message())?,
        }
    }
    Ok(())
}
