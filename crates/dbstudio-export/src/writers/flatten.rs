//! Flattening of document sources (MongoDB `$document` column) for the tabular formats.
//!
//! [`FlattenWriter`] wraps the CSV / XLSX writer. When an object arrives with the single
//! `$document` column, every document becomes one row: either the configured dotted paths
//! (`a.b.c`, `items.0.name`) or the union of the top-level keys of the first
//! [`SAMPLE_DOCUMENTS`] documents (`_id` first). Sub-documents and arrays that are not
//! flattened further are written as relaxed Extended JSON text. Any other object is passed
//! through untouched.

use std::path::PathBuf;

use dbstudio_core::{
    ColumnMeta, DdlItem, DdlSection, Result, ResultSink, StatementEnd, Value, ValueKind,
};

use crate::values::write_json;
use crate::{BinaryFormat, ExtendedJson, ObjectMeta, TableWriter};

/// Name of the column document sources return.
pub(crate) const DOC_COLUMN: &str = "$document";
/// Documents inspected for the header when no path is configured.
pub(crate) const SAMPLE_DOCUMENTS: usize = 1000;
/// Warnings about late keys are capped; the rest is summarised.
const MAX_LATE_WARNINGS: usize = 20;

pub(crate) struct FlattenWriter {
    inner: Box<dyn TableWriter>,
    /// Configured paths; empty = detect from the first documents.
    paths: Vec<String>,
    binary: BinaryFormat,
    state: State,
    warnings: Vec<String>,
    late_warnings: usize,
}

enum State {
    /// The current object is not a document source.
    Passthrough,
    /// No object is open.
    Idle,
    /// Collecting the first documents to learn the header.
    Sampling {
        meta: Box<ObjectMeta>,
        docs: Vec<Vec<Value>>,
    },
    /// Header known: documents are mapped onto `keys`.
    Flattening {
        keys: Vec<String>,
        /// Paths are used as typed (configured) rather than as literal top-level keys.
        by_path: bool,
        /// Rows seen so far (1-based number of the next row minus one).
        seen: u64,
        /// Top-level keys already reported as missing from the header.
        reported: std::collections::HashSet<String>,
    },
}

impl FlattenWriter {
    pub fn new(inner: Box<dyn TableWriter>, paths: Vec<String>, binary: BinaryFormat) -> Self {
        FlattenWriter {
            inner,
            paths,
            binary,
            state: State::Idle,
            warnings: Vec::new(),
            late_warnings: 0,
        }
    }

    fn is_document_source(meta: &ObjectMeta) -> bool {
        meta.columns.len() == 1 && meta.columns[0].name == DOC_COLUMN
    }

    /// `meta` describing the flattened table.
    fn flat_meta(meta: &ObjectMeta, keys: &[String]) -> ObjectMeta {
        let columns: Vec<ColumnMeta> = keys
            .iter()
            .map(|k| ColumnMeta {
                name: k.clone(),
                type_name: "document field".into(),
                value_kind: ValueKind::Text,
                nullable: Some(true),
            })
            .collect();
        ObjectMeta {
            path: meta.path.clone(),
            name: meta.name.clone(),
            columns,
            // The model describes the single `$document` column; the dictionary of the
            // flattened file lists the flattened columns instead.
            details: None,
            estimated_rows: meta.estimated_rows,
        }
    }

    fn flat_row(&self, doc: &Value, keys: &[String], by_path: bool) -> Vec<Value> {
        keys.iter()
            .map(|k| {
                let found = if by_path {
                    resolve_path(doc, k)
                } else {
                    top_level(doc, k)
                };
                match found {
                    Some(v) => leaf(v, self.binary),
                    None => Value::Null,
                }
            })
            .collect()
    }

    /// Learn the header from the sampled documents, open the inner object and flush them.
    fn start_flattening(&mut self, meta: ObjectMeta, docs: Vec<Vec<Value>>) -> Result<()> {
        let (keys, by_path) = if self.paths.is_empty() {
            (detect_keys(&docs), false)
        } else {
            (self.paths.clone(), true)
        };
        // A document table with no document at all still needs a column.
        let keys = if keys.is_empty() {
            vec!["_id".to_string()]
        } else {
            keys
        };
        let flat = Self::flat_meta(&meta, &keys);
        self.inner.begin_object(&flat)?;
        let mut seen = 0u64;
        let mut reported = std::collections::HashSet::new();
        let mut rows = Vec::with_capacity(docs.len());
        for row in &docs {
            seen += 1;
            if let Some(doc) = row.first() {
                if !by_path {
                    self.note_late_keys(doc, &keys, seen, &mut reported);
                }
                rows.push(self.flat_row(doc, &keys, by_path));
            }
        }
        if !rows.is_empty() {
            self.inner.on_rows(rows)?;
        }
        self.state = State::Flattening {
            keys,
            by_path,
            seen,
            reported,
        };
        Ok(())
    }

    /// Keys of `doc` that are not in the header (they first appear after the sample).
    fn note_late_keys(
        &mut self,
        doc: &Value,
        keys: &[String],
        row: u64,
        reported: &mut std::collections::HashSet<String>,
    ) {
        let Value::Document(fields) = doc else {
            return;
        };
        for (k, _) in fields {
            if keys.contains(k) || !reported.insert(k.clone()) {
                continue;
            }
            self.late_warnings += 1;
            if self.late_warnings <= MAX_LATE_WARNINGS {
                self.warnings
                    .push(format!("field {k} first seen at row {row}: not exported"));
            }
        }
    }
}

/// Keys of the documents in first-seen order, `_id` first.
fn detect_keys(docs: &[Vec<Value>]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in docs {
        if let Some(Value::Document(fields)) = row.first() {
            for (k, _) in fields {
                if seen.insert(k.clone()) {
                    keys.push(k.clone());
                }
            }
        }
    }
    if let Some(i) = keys.iter().position(|k| k == "_id") {
        let id = keys.remove(i);
        keys.insert(0, id);
    }
    keys
}

fn top_level<'a>(doc: &'a Value, key: &str) -> Option<&'a Value> {
    match doc {
        Value::Document(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}

/// Value at a dotted path: `a.b.c`, array elements by index (`items.0.name`). A top-level key
/// that itself contains dots is found first.
pub(crate) fn resolve_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    if let Some(v) = top_level(doc, path) {
        return Some(v);
    }
    let mut cur = doc;
    let mut rest = path;
    while !rest.is_empty() {
        match cur {
            Value::Document(fields) => {
                // The longest key that matches the start of `rest` (keys may contain dots).
                let hit = fields
                    .iter()
                    .filter(|(k, _)| {
                        rest == k.as_str()
                            || rest
                                .strip_prefix(k.as_str())
                                .is_some_and(|r| r.starts_with('.'))
                    })
                    .max_by_key(|(k, _)| k.len());
                let (k, v) = hit?;
                rest = rest[k.len()..].trim_start_matches('.');
                cur = v;
            }
            Value::Array(items) => {
                let (head, tail) = rest.split_once('.').unwrap_or((rest, ""));
                cur = items.get(head.parse::<usize>().ok()?)?;
                rest = tail;
            }
            _ => return None,
        }
    }
    Some(cur)
}

/// Cell for a value found in a document: scalars stay typed, documents and arrays become
/// relaxed EJSON text.
fn leaf(v: &Value, binary: BinaryFormat) -> Value {
    match v {
        Value::Document(_) | Value::Array(_) => {
            let mut s = String::new();
            write_json(&mut s, v, binary, ExtendedJson::Relaxed, None, 0);
            Value::Text(s)
        }
        other => other.clone(),
    }
}

impl ResultSink for FlattenWriter {
    fn on_columns(&mut self, columns: Vec<ColumnMeta>) -> Result<()> {
        self.inner.on_columns(columns)
    }

    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Passthrough => {
                self.state = State::Passthrough;
                self.inner.on_rows(rows)
            }
            State::Idle => {
                self.state = State::Idle;
                self.inner.on_rows(rows)
            }
            State::Sampling { meta, mut docs } => {
                docs.extend(rows);
                if self.paths.is_empty() && docs.len() < SAMPLE_DOCUMENTS {
                    self.state = State::Sampling { meta, docs };
                    return Ok(());
                }
                if self.paths.is_empty() {
                    // Rows beyond the sample are mapped like any later row.
                    let rest = docs.split_off(SAMPLE_DOCUMENTS);
                    self.start_flattening(*meta, docs)?;
                    return self.on_rows(rest);
                }
                self.start_flattening(*meta, docs)
            }
            State::Flattening {
                keys,
                by_path,
                mut seen,
                mut reported,
            } => {
                let mut out = Vec::with_capacity(rows.len());
                for row in &rows {
                    seen += 1;
                    if let Some(doc) = row.first() {
                        if !by_path {
                            self.note_late_keys(doc, &keys, seen, &mut reported);
                        }
                        out.push(self.flat_row(doc, &keys, by_path));
                    }
                }
                self.state = State::Flattening {
                    keys,
                    by_path,
                    seen,
                    reported,
                };
                if out.is_empty() {
                    Ok(())
                } else {
                    self.inner.on_rows(out)
                }
            }
        }
    }

    fn on_message(&mut self, message: String) -> Result<()> {
        self.inner.on_message(message)
    }

    fn on_statement_end(&mut self, end: StatementEnd) -> Result<()> {
        self.inner.on_statement_end(end)
    }
}

impl TableWriter for FlattenWriter {
    fn begin_object(&mut self, meta: &ObjectMeta) -> Result<()> {
        if !Self::is_document_source(meta) {
            self.state = State::Passthrough;
            return self.inner.begin_object(meta);
        }
        if self.paths.is_empty() {
            self.state = State::Sampling {
                meta: Box::new(meta.clone()),
                docs: Vec::new(),
            };
            Ok(())
        } else {
            // Header known up front: open the object right away (no buffering).
            let paths = self.paths.clone();
            let flat = Self::flat_meta(meta, &paths);
            self.inner.begin_object(&flat)?;
            self.state = State::Flattening {
                keys: paths,
                by_path: true,
                seen: 0,
                reported: Default::default(),
            };
            Ok(())
        }
    }

    fn end_object(&mut self) -> Result<()> {
        if let State::Sampling { meta, docs } = std::mem::replace(&mut self.state, State::Idle) {
            self.start_flattening(*meta, docs)?;
        }
        self.state = State::Idle;
        self.inner.end_object()
    }

    fn finish(self: Box<Self>) -> Result<Vec<PathBuf>> {
        self.inner.finish()
    }

    fn write_ddl(&mut self, section: DdlSection, items: &[DdlItem]) -> Result<()> {
        self.inner.write_ddl(section, items)
    }

    fn bytes(&self) -> u64 {
        self.inner.bytes()
    }

    fn warnings(&mut self) -> Vec<String> {
        let mut out = std::mem::take(&mut self.warnings);
        if self.late_warnings > MAX_LATE_WARNINGS {
            out.push(format!(
                "{} more field(s) first seen after the first {SAMPLE_DOCUMENTS} documents were not exported",
                self.late_warnings - MAX_LATE_WARNINGS
            ));
        }
        out.extend(self.inner.warnings());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::ExtValue;
    use std::sync::{Arc, Mutex};

    fn d(fields: Vec<(&str, Value)>) -> Value {
        Value::Document(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    #[derive(Default)]
    struct Log {
        headers: Vec<Vec<String>>,
        rows: Vec<Vec<Value>>,
        ended: u32,
    }

    struct Probe(Arc<Mutex<Log>>);

    impl ResultSink for Probe {
        fn on_columns(&mut self, _c: Vec<ColumnMeta>) -> Result<()> {
            Ok(())
        }
        fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
            self.0.lock().unwrap().rows.extend(rows);
            Ok(())
        }
        fn on_message(&mut self, _m: String) -> Result<()> {
            Ok(())
        }
        fn on_statement_end(&mut self, _e: StatementEnd) -> Result<()> {
            Ok(())
        }
    }

    impl TableWriter for Probe {
        fn begin_object(&mut self, meta: &ObjectMeta) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .headers
                .push(meta.columns.iter().map(|c| c.name.clone()).collect());
            Ok(())
        }
        fn end_object(&mut self) -> Result<()> {
            self.0.lock().unwrap().ended += 1;
            Ok(())
        }
        fn finish(self: Box<Self>) -> Result<Vec<PathBuf>> {
            Ok(vec![])
        }
    }

    fn doc_meta() -> ObjectMeta {
        ObjectMeta {
            path: None,
            name: "c".into(),
            columns: vec![ColumnMeta {
                name: DOC_COLUMN.into(),
                type_name: "document".into(),
                value_kind: ValueKind::Document,
                nullable: Some(false),
            }],
            details: None,
            estimated_rows: None,
        }
    }

    fn writer(paths: &[&str]) -> (FlattenWriter, Arc<Mutex<Log>>) {
        let log = Arc::new(Mutex::new(Log::default()));
        let w = FlattenWriter::new(
            Box::new(Probe(Arc::clone(&log))),
            paths.iter().map(|p| p.to_string()).collect(),
            BinaryFormat::Base64,
        );
        (w, log)
    }

    #[test]
    fn paths_resolve_nested_fields_and_array_indexes() {
        let doc = d(vec![
            ("a", d(vec![("b", d(vec![("c", Value::Int(7))]))])),
            (
                "items",
                Value::Array(vec![
                    d(vec![("name", text("x"))]),
                    d(vec![("name", text("y"))]),
                ]),
            ),
            ("dotted.key", Value::Int(1)),
        ]);
        assert_eq!(resolve_path(&doc, "a.b.c"), Some(&Value::Int(7)));
        assert_eq!(resolve_path(&doc, "items.1.name"), Some(&text("y")));
        assert_eq!(resolve_path(&doc, "items.5.name"), None);
        assert_eq!(resolve_path(&doc, "a.x"), None);
        assert_eq!(resolve_path(&doc, "dotted.key"), Some(&Value::Int(1)));
        assert_eq!(resolve_path(&doc, "a.b.c.d"), None);
    }

    #[test]
    fn heterogeneous_documents_get_the_key_union_with_id_first() {
        let (mut w, log) = writer(&[]);
        w.begin_object(&doc_meta()).unwrap();
        w.on_rows(vec![
            vec![d(vec![
                ("name", text("a")),
                ("_id", Value::Ext(ExtValue::new("objectId", "507f"))),
            ])],
            vec![d(vec![
                ("_id", Value::Int(2)),
                ("tags", Value::Array(vec![text("t")])),
                ("meta", d(vec![("k", Value::Int(1))])),
            ])],
        ])
        .unwrap();
        // Nothing is written before the object ends (header not final yet).
        assert!(log.lock().unwrap().headers.is_empty());
        w.end_object().unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.headers, [["_id", "name", "tags", "meta"]]);
        assert_eq!(
            log.rows[0][0],
            Value::Ext(ExtValue::new("objectId", "507f"))
        );
        assert_eq!(log.rows[0][1], text("a"));
        assert_eq!(log.rows[0][2], Value::Null);
        assert_eq!(log.rows[1][2], text("[\"t\"]"));
        assert_eq!(log.rows[1][3], text("{\"k\":1}"));
        assert_eq!(log.ended, 1);
    }

    #[test]
    fn configured_paths_open_the_object_at_once() {
        let (mut w, log) = writer(&["_id", "a.b", "items.0.name", "missing"]);
        w.begin_object(&doc_meta()).unwrap();
        assert_eq!(log.lock().unwrap().headers.len(), 1);
        w.on_rows(vec![vec![d(vec![
            ("_id", Value::Int(1)),
            ("a", d(vec![("b", d(vec![("c", Value::Int(1))]))])),
            ("items", Value::Array(vec![d(vec![("name", text("n"))])])),
        ])]])
        .unwrap();
        w.end_object().unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.headers[0], ["_id", "a.b", "items.0.name", "missing"]);
        assert_eq!(
            log.rows[0],
            [Value::Int(1), text("{\"c\":1}"), text("n"), Value::Null]
        );
    }

    #[test]
    fn keys_first_seen_after_the_sample_are_dropped_with_a_warning() {
        let (mut w, log) = writer(&[]);
        w.begin_object(&doc_meta()).unwrap();
        let rows: Vec<Vec<Value>> = (0..SAMPLE_DOCUMENTS + 5)
            .map(|i| {
                let mut f = vec![("_id", Value::Int(i as i64))];
                if i == SAMPLE_DOCUMENTS + 2 {
                    f.push(("late", Value::Int(1)));
                }
                vec![d(f)]
            })
            .collect();
        // Delivered in two batches to cross the sample boundary mid-batch.
        let (a, b) = rows.split_at(SAMPLE_DOCUMENTS - 10);
        w.on_rows(a.to_vec()).unwrap();
        w.on_rows(b.to_vec()).unwrap();
        w.end_object().unwrap();
        let warnings = w.warnings();
        assert_eq!(
            warnings,
            [format!(
                "field late first seen at row {}: not exported",
                SAMPLE_DOCUMENTS + 3
            )]
        );
        let log = log.lock().unwrap();
        assert_eq!(log.headers, [["_id"]]);
        assert_eq!(log.rows.len(), SAMPLE_DOCUMENTS + 5);
    }

    #[test]
    fn other_objects_pass_through() {
        let (mut w, log) = writer(&["x"]);
        let mut meta = doc_meta();
        meta.columns[0].name = "id".into();
        w.begin_object(&meta).unwrap();
        w.on_rows(vec![vec![Value::Int(1)]]).unwrap();
        w.end_object().unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.headers, [["id"]]);
        assert_eq!(log.rows, [[Value::Int(1)]]);
    }

    #[test]
    fn empty_collection_still_gets_a_header() {
        let (mut w, log) = writer(&[]);
        w.begin_object(&doc_meta()).unwrap();
        w.end_object().unwrap();
        assert_eq!(log.lock().unwrap().headers, [["_id"]]);
    }
}
