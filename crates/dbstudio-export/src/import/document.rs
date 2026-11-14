//! Import into a MongoDB collection.
//!
//! JSON / NDJSON: every record becomes one whole document (the record text goes to the driver
//! as Extended JSON through the `$document` column convention of `BulkInsertRequest`).
//! CSV / XLSX: columns map to field paths (`a.b.c` builds nested documents); text cells that are
//! JSON scalars (numbers, booleans, `null`, EJSON wrappers such as `{"$oid": ...}`) become typed
//! values, everything else stays a string. Empty cells are left out of the document.

use std::sync::Arc;

use dbstudio_core::{
    BulkInsertRequest, CancelToken, EditValue, Error, JobProgress, ObjectKind, ObjectPath,
    OnConflict, QueryRequest, Result, TxOptions, Value,
};

use super::source::{self, JsonRecords, RowSource};
use super::{apply_transform, find_ci, map_cancel};
use crate::conn::{Conn, database_of};
use crate::values::json_string;
use crate::{
    ColumnMapping, ExportContext, ImportFormat, ImportMode, ImportReport, ImportSpec, ImportTarget,
    RowError, Transform,
};

/// Documents per `bulk_insert` call.
const MAX_BATCH: usize = 1000;
/// Column convention of whole-document rows.
const DOC_COLUMN: &str = "$document";

// ---------------------------------------------------------------------------------------------
// Documents from rows
// ---------------------------------------------------------------------------------------------

/// A field of the document under construction.
#[derive(Debug)]
enum Node {
    Leaf(String),
    Doc(Vec<(String, Node)>),
}

fn insert_path(fields: &mut Vec<(String, Node)>, path: &[&str], literal: String) {
    let [head, rest @ ..] = path else {
        return;
    };
    if rest.is_empty() {
        match fields.iter_mut().find(|(k, _)| k == head) {
            Some((_, n)) => *n = Node::Leaf(literal),
            None => fields.push(((*head).to_string(), Node::Leaf(literal))),
        }
        return;
    }
    let i = match fields.iter().position(|(k, _)| k == head) {
        Some(i) => i,
        None => {
            fields.push(((*head).to_string(), Node::Doc(Vec::new())));
            fields.len() - 1
        }
    };
    if !matches!(fields[i].1, Node::Doc(_)) {
        fields[i].1 = Node::Doc(Vec::new());
    }
    if let Node::Doc(inner) = &mut fields[i].1 {
        insert_path(inner, rest, literal);
    }
}

fn write_node(out: &mut String, fields: &[(String, Node)]) {
    out.push('{');
    for (i, (k, n)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_string(out, k);
        out.push(':');
        match n {
            Node::Leaf(l) => out.push_str(l),
            Node::Doc(inner) => write_node(out, inner),
        }
    }
    out.push('}');
}

/// Extended JSON text of a cell; `None` = leave the field out.
fn leaf_json(cell: &EditValue) -> Option<String> {
    let mut out = String::new();
    match cell {
        EditValue::Default | EditValue::Value(Value::Null) => return None,
        EditValue::Text(t) => text_leaf(t, &mut out),
        EditValue::Value(v) => match v {
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Int(i) => out.push_str(&i.to_string()),
            Value::UInt(u) => out.push_str(&u.to_string()),
            Value::Float(f) if f.is_finite() => out.push_str(&format!("{f:?}")),
            Value::Float(f) => json_string(&mut out, &f.to_string()),
            Value::Decimal(d) => {
                out.push_str("{\"$numberDecimal\":");
                json_string(&mut out, d);
                out.push('}');
            }
            Value::Date(d) => date_json(&mut out, &format!("{d}T00:00:00Z")),
            Value::DateTime(d) => date_json(&mut out, &format!("{}Z", d.replace(' ', "T"))),
            Value::Timestamp(d) => date_json(&mut out, d),
            Value::Json(j) => out.push_str(j.trim()),
            Value::Bytes(b) => {
                use base64::Engine as _;
                out.push_str("{\"$binary\":{\"base64\":");
                json_string(
                    &mut out,
                    &base64::engine::general_purpose::STANDARD.encode(b),
                );
                out.push_str(",\"subType\":\"00\"}}");
            }
            Value::Text(s) | Value::Time(s) | Value::Uuid(s) => json_string(&mut out, s),
            Value::Ext(e) => json_string(&mut out, &e.text),
            Value::Array(_) | Value::Document(_) | Value::Null => return None,
        },
    }
    Some(out)
}

fn date_json(out: &mut String, iso: &str) {
    out.push_str("{\"$date\":");
    json_string(out, iso);
    out.push('}');
}

/// Keys of the Extended JSON scalar wrappers a text cell may carry (`{"$oid": "..."}`).
const EJSON_WRAPPERS: [&str; 15] = [
    "$oid",
    "$date",
    "$numberInt",
    "$numberLong",
    "$numberDouble",
    "$numberDecimal",
    "$binary",
    "$uuid",
    "$timestamp",
    "$regularExpression",
    "$symbol",
    "$code",
    "$minKey",
    "$maxKey",
    "$undefined",
];

/// The value is plain JSON, or uses only Extended JSON scalar wrappers for its `$` keys.
/// Operators such as `{"$ne": 1}` or `{"a": {"$gt": 1}}` are not accepted anywhere.
fn ejson_safe(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Array(items) => items.iter().all(ejson_safe),
        serde_json::Value::Object(o) => {
            if o.len() == 1 && o.keys().all(|k| EJSON_WRAPPERS.contains(&k.as_str())) {
                return true;
            }
            o.iter().all(|(k, v)| !k.starts_with('$') && ejson_safe(v))
        }
        _ => true,
    }
}

/// Whether a text cell is a JSON literal that becomes a typed value: numbers (integers only when
/// they fit 64 bits), booleans, `null`, Extended JSON wrappers, and objects / arrays built from
/// them (what the CSV / XLSX export writes for sub-documents and arrays). Anything else is a
/// string.
fn is_json_literal(trimmed: &str) -> bool {
    let digits = trimmed.strip_prefix('-').unwrap_or(trimmed);
    let integer = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(serde_json::Value::Number(_)) => !integer || trimmed.parse::<i64>().is_ok(),
        Ok(serde_json::Value::Bool(_) | serde_json::Value::Null) => true,
        Ok(v @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) => ejson_safe(&v),
        _ => false,
    }
}

/// A text cell as a JSON literal when it is one (see [`is_json_literal`]), otherwise as a string.
fn text_leaf(t: &str, out: &mut String) {
    let trimmed = t.trim();
    if is_json_literal(trimmed) {
        out.push_str(trimmed);
    } else {
        json_string(out, t);
    }
}

/// The document of one row.
fn document_text(paths: &[(usize, Vec<String>, Transform)], cells: &[EditValue]) -> String {
    let mut fields: Vec<(String, Node)> = Vec::new();
    for (index, path, transform) in paths {
        let cell = cells
            .get(*index)
            .cloned()
            .unwrap_or(EditValue::Value(Value::Null));
        let cell = apply_transform(cell, *transform);
        let Some(literal) = leaf_json(&cell) else {
            continue;
        };
        let segments: Vec<&str> = path.iter().map(String::as_str).collect();
        insert_path(&mut fields, &segments, literal);
    }
    let mut out = String::new();
    write_node(&mut out, &fields);
    out
}

/// Column → field path plan. `a` and `a.b` together cannot both be fields.
fn plan_paths(
    mapping: &[ColumnMapping],
    file_columns: &[String],
) -> Result<Vec<(usize, Vec<String>, Transform)>> {
    let pairs: Vec<(usize, String, Transform)> = if mapping.is_empty() {
        file_columns
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.clone(), Transform::None))
            .collect()
    } else {
        let mut out = Vec::new();
        for m in mapping {
            let index = match m
                .source
                .strip_prefix('#')
                .and_then(|n| n.parse::<usize>().ok())
            {
                Some(n) if n >= 1 && n <= file_columns.len() => n - 1,
                Some(_) => {
                    return Err(Error::Invalid(format!(
                        "column {} is outside the file's {} column(s)",
                        m.source,
                        file_columns.len()
                    )));
                }
                None => find_ci(file_columns, &m.source).ok_or_else(|| {
                    Error::Invalid(format!("column not found in the file: {}", m.source))
                })?,
            };
            out.push((index, m.target.clone(), m.transform));
        }
        out
    };
    let mut out: Vec<(usize, Vec<String>, Transform)> = Vec::new();
    for (index, target, transform) in pairs {
        let path: Vec<String> = target.split('.').map(str::to_string).collect();
        if path.iter().any(String::is_empty) || target.starts_with('$') {
            return Err(Error::Invalid(format!("invalid field name: {target}")));
        }
        for (_, other, _) in &out {
            let n = path.len().min(other.len());
            if path[..n] == other[..n] {
                return Err(Error::Invalid(format!(
                    "field {target} conflicts with {}",
                    other.join(".")
                )));
            }
        }
        out.push((index, path, transform));
    }
    if out.is_empty() {
        return Err(Error::Invalid("no column to import".into()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------------------------

/// A pending document and the line it came from.
struct Pending {
    line: u64,
    /// The whole document as Extended JSON (`$document` mode).
    text: String,
    /// Cells for the column mode of `Update` (one per mapped path).
    cells: Option<Vec<EditValue>>,
}

/// Where documents come from.
enum Input {
    Records(JsonRecords),
    Rows {
        src: Box<dyn RowSource>,
        paths: Vec<(usize, Vec<String>, Transform)>,
        /// `Update` from CSV / XLSX: send plain columns so the driver `$set`s the given paths
        /// and keeps every other field of the document.
        columns_mode: bool,
    },
}

impl Input {
    fn next(&mut self) -> Result<Option<Pending>> {
        match self {
            Input::Records(r) => Ok(r.next()?.map(|(line, text)| Pending {
                line,
                text: text.trim().to_string(),
                cells: None,
            })),
            Input::Rows {
                src,
                paths,
                columns_mode,
            } => Ok(src.next_row()?.map(|row| {
                if *columns_mode {
                    let cells = paths
                        .iter()
                        .map(|(i, _, t)| {
                            let cell = row
                                .cells
                                .get(*i)
                                .cloned()
                                .unwrap_or(EditValue::Value(Value::Null));
                            match apply_transform(cell, *t) {
                                EditValue::Value(Value::Null) => EditValue::Default,
                                // Same typing as the `$document` mode.
                                EditValue::Text(t) if is_json_literal(t.trim()) => {
                                    EditValue::Value(Value::Json(t.trim().to_string()))
                                }
                                other => other,
                            }
                        })
                        .collect();
                    Pending {
                        line: row.line,
                        text: String::new(),
                        cells: Some(cells),
                    }
                } else {
                    Pending {
                        line: row.line,
                        text: document_text(paths, &row.cells),
                        cells: None,
                    }
                }
            })),
        }
    }

    /// Dotted field paths of the column mode.
    fn update_columns(&self) -> Option<Vec<String>> {
        match self {
            Input::Rows {
                paths,
                columns_mode: true,
                ..
            } => Some(paths.iter().map(|(_, p, _)| p.join(".")).collect()),
            _ => None,
        }
    }
}

/// `row N: message` of the driver's batch error → `(N, message)`.
fn failing_row(message: &str) -> Option<(usize, String)> {
    let rest = message.split("row ").nth(1)?;
    let (n, msg) = rest.split_once(':')?;
    Some((n.trim().parse().ok()?, msg.trim().to_string()))
}

pub(crate) async fn run(
    ctx: &ExportContext,
    spec: &ImportSpec,
    cancel: &CancelToken,
) -> Result<ImportReport> {
    let path = std::path::PathBuf::from(&spec.file);
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    // Target collection.
    let (collection, create): (ObjectPath, Option<String>) = match &spec.target {
        ImportTarget::Existing { table } => {
            if !table.segments.iter().any(|s| {
                matches!(
                    s.kind,
                    ObjectKind::Collection | ObjectKind::Table | ObjectKind::View
                )
            }) {
                return Err(Error::Invalid("the target is not a collection".into()));
            }
            (table.clone(), None)
        }
        ImportTarget::New { parent, name, .. } => {
            if name.is_empty() {
                return Err(Error::Invalid("the new collection has no name".into()));
            }
            let mut text = String::from("db.createCollection(");
            json_string(&mut text, name);
            text.push(')');
            (
                parent.clone().child(ObjectKind::Collection, name),
                Some(text),
            )
        }
        ImportTarget::Script { .. } | ImportTarget::Keys { .. } => {
            return Err(Error::Invalid(
                "a document import needs a collection as target".into(),
            ));
        }
    };

    let on_conflict = spec.on_conflict.unwrap_or(OnConflict::Error);
    let single = matches!(spec.mode, ImportMode::SingleTransaction);
    // A failed write aborts a MongoDB transaction: a failing document cannot be skipped or
    // tolerated inside one, so these combinations are refused before anything runs.
    if single && (on_conflict == OnConflict::Skip || spec.max_errors > 0) {
        return Err(Error::Invalid(
            "a single transaction cannot skip duplicates or tolerate errors: use batches, or \
             set `max_errors` to 0 with the default conflict policy"
                .into(),
        ));
    }
    let columns_mode = on_conflict == OnConflict::Update;
    let (open_path, format) = (path.clone(), spec.format.clone());
    let mapping = spec.mapping.clone();
    let mut input = tokio::task::spawn_blocking(move || -> Result<Input> {
        Ok(match &format {
            ImportFormat::Json { layout } => {
                Input::Records(JsonRecords::open(&open_path, *layout)?)
            }
            ImportFormat::Csv { .. } | ImportFormat::Xlsx { .. } => {
                let src = source::open(&open_path, &format, false)?;
                let paths = plan_paths(&mapping, src.columns())?;
                Input::Rows {
                    src,
                    paths,
                    columns_mode,
                }
            }
            ImportFormat::Sql { .. } => {
                return Err(Error::Invalid("a SQL script has no rows".into()));
            }
        })
    })
    .await
    .map_err(|e| Error::Other(format!("import task failed: {e}")))??;

    let update_columns = input.update_columns();
    let database = database_of(&collection);
    let (conn, _) = Conn::open(Arc::clone(&ctx.session), database.clone(), None).await?;
    let conn = Arc::new(conn);
    let batch_rows = match spec.mode {
        ImportMode::SingleTransaction => MAX_BATCH,
        ImportMode::Batches { rows } => (rows.max(1) as usize).min(MAX_BATCH),
    };
    let mut state = State {
        ctx,
        conn: &conn,
        cancel,
        collection,
        on_conflict,
        update_columns,
        max_errors: spec.max_errors,
        report: ImportReport::default(),
        progress: JobProgress {
            phase: "import".into(),
            object: Some("documents".into()),
            bytes_total: Some(size),
            ..JobProgress::default()
        },
    };
    let result = async {
        if let Some(text) = create {
            conn.execute_cancellable(cancel, QueryRequest::new(text), &mut Discard)
                .await?;
        }
        if single {
            let lease = conn
                .lease()
                .ok_or_else(|| Error::Unsupported("leases".into()))?;
            lease.begin(TxOptions::default()).await?;
        }
        let loaded = state.load_all(&mut input, batch_rows).await;
        if single {
            if let Some(lease) = conn.lease() {
                match &loaded {
                    Ok(()) => lease.commit().await?,
                    Err(_) => {
                        let _ = lease.rollback().await;
                    }
                }
            }
        }
        loaded
    }
    .await;
    let report = std::mem::take(&mut state.report);
    conn.close().await;
    result.map(|()| report)
}

/// Sink that drops everything (`createCollection` has no rows).
struct Discard;

impl dbstudio_core::ResultSink for Discard {
    fn on_columns(&mut self, _c: Vec<dbstudio_core::ColumnMeta>) -> Result<()> {
        Ok(())
    }
    fn on_rows(&mut self, _r: Vec<Vec<Value>>) -> Result<()> {
        Ok(())
    }
    fn on_message(&mut self, _m: String) -> Result<()> {
        Ok(())
    }
    fn on_statement_end(&mut self, _e: dbstudio_core::StatementEnd) -> Result<()> {
        Ok(())
    }
}

struct State<'a> {
    ctx: &'a ExportContext,
    conn: &'a Arc<Conn>,
    cancel: &'a CancelToken,
    collection: ObjectPath,
    on_conflict: OnConflict,
    /// Field paths when rows go as plain columns (`Update` from CSV / XLSX).
    update_columns: Option<Vec<String>>,
    max_errors: u32,
    report: ImportReport,
    progress: JobProgress,
}

impl State<'_> {
    async fn load_all(&mut self, input: &mut Input, batch_rows: usize) -> Result<()> {
        loop {
            if self.cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let mut batch: Vec<Pending> = Vec::with_capacity(batch_rows.min(1000));
            let mut eof = false;
            while batch.len() < batch_rows {
                match input.next()? {
                    Some(p) => {
                        self.report.rows_read += 1;
                        batch.push(p);
                    }
                    None => {
                        eof = true;
                        break;
                    }
                }
            }
            if !batch.is_empty() {
                self.insert(batch).await?;
                self.progress.rows_done = self.report.rows_read;
                self.ctx.reporter.progress(&self.progress);
            }
            if eof {
                return Ok(());
            }
        }
    }

    /// Insert a batch. A failing document (duplicate `_id`, bad value) is reported with its
    /// line; the rows before it were written (ordered insert) and the rest is sent again.
    async fn insert(&mut self, mut batch: Vec<Pending>) -> Result<()> {
        while !batch.is_empty() {
            let rows: Vec<Vec<EditValue>> = batch
                .iter()
                .map(|p| match &p.cells {
                    Some(cells) => cells.clone(),
                    None => vec![EditValue::Value(Value::Json(p.text.clone()))],
                })
                .collect();
            let columns = self
                .update_columns
                .clone()
                .unwrap_or_else(|| vec![DOC_COLUMN.to_string()]);
            let lease = self
                .conn
                .lease()
                .ok_or_else(|| Error::Unsupported("leases".into()))?;
            let req = BulkInsertRequest {
                table: self.collection.clone(),
                columns,
                rows,
                on_conflict: self.on_conflict,
                conflict_columns: Vec::new(),
            };
            match lease.bulk_insert(req).await {
                Ok(r) => {
                    self.report.inserted += r.inserted;
                    self.report.updated += r.updated;
                    self.report.skipped += r.skipped;
                    return Ok(());
                }
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => {
                    let e = map_cancel(e, self.cancel);
                    let message = e.message();
                    let Some((row, why)) = failing_row(&message).filter(|(n, _)| {
                        *n >= 1 && *n <= batch.len() && self.on_conflict != OnConflict::Update
                    }) else {
                        // Not attributable to one document: the import stops.
                        return Err(e);
                    };
                    // `row` is 1-based: the rows before it were inserted.
                    self.report.inserted += (row - 1) as u64;
                    let failed = batch.remove(row - 1);
                    batch.drain(..row - 1);
                    self.report.errors.push(RowError {
                        line: failed.line,
                        message: why.clone(),
                    });
                    if self.report.errors.len() as u32 > self.max_errors {
                        return Err(Error::Query {
                            message: format!("line {}: {why}", failed.line),
                            position: None,
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> EditValue {
        EditValue::Text(s.into())
    }

    fn plan(targets: &[&str]) -> Vec<(usize, Vec<String>, Transform)> {
        targets
            .iter()
            .enumerate()
            .map(|(i, t)| {
                (
                    i,
                    t.split('.').map(str::to_string).collect(),
                    Transform::None,
                )
            })
            .collect()
    }

    #[test]
    fn dotted_targets_build_nested_documents() {
        let doc = document_text(
            &plan(&[
                "name",
                "address.city",
                "address.geo.lat",
                "age",
                "ok",
                "code",
            ]),
            &[
                text("Ann \"A\""),
                text("Hue"),
                text("16.46"),
                text("30"),
                text("true"),
                text("007"),
            ],
        );
        assert_eq!(
            doc,
            r#"{"name":"Ann \"A\"","address":{"city":"Hue","geo":{"lat":16.46}},"age":30,"ok":true,"code":"007"}"#
        );
        let parsed: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(parsed["address"]["geo"]["lat"], 16.46);
    }

    #[test]
    fn ejson_wrappers_empty_cells_and_typed_values() {
        let doc = document_text(
            &plan(&["_id", "gone", "n", "when", "price", "txt"]),
            &[
                text(r#"{"$oid":"507f1f77bcf86cd799439011"}"#),
                EditValue::Value(Value::Null),
                EditValue::Value(Value::Int(5)),
                EditValue::Value(Value::DateTime("2024-05-01 10:00:00".into())),
                EditValue::Value(Value::Decimal("1.50".into())),
                text(r#"{"not":"a wrapper"}"#),
            ],
        );
        assert_eq!(
            doc,
            r#"{"_id":{"$oid":"507f1f77bcf86cd799439011"},"n":5,"when":{"$date":"2024-05-01T10:00:00Z"},"price":{"$numberDecimal":"1.50"},"txt":"{\"not\":\"a wrapper\"}"}"#
        );
    }

    #[test]
    fn mapping_is_planned_by_name_or_position_and_conflicts_are_refused() {
        let file = vec!["id".to_string(), "City".to_string()];
        let p = plan_paths(&[], &file).unwrap();
        assert_eq!(p.len(), 2);
        let m = |s: &str, t: &str| ColumnMapping {
            source: s.into(),
            target: t.into(),
            transform: Transform::Trim,
        };
        let p = plan_paths(&[m("#2", "a.b"), m("id", "_id")], &file).unwrap();
        assert_eq!((p[0].0, p[0].1.clone()), (1, vec!["a".into(), "b".into()]));
        assert_eq!(p[1].2, Transform::Trim);
        assert!(plan_paths(&[m("id", "a"), m("City", "a.b")], &file).is_err());
        assert!(plan_paths(&[m("id", "a..b")], &file).is_err());
        assert!(plan_paths(&[m("id", "$x")], &file).is_err());
        assert!(plan_paths(&[m("zz", "a")], &file).is_err());
        assert!(plan_paths(&[m("#9", "a")], &file).is_err());
    }

    #[test]
    fn driver_row_errors_are_parsed() {
        assert_eq!(
            failing_row("row 3: E11000 duplicate key (code 11000)"),
            Some((3, "E11000 duplicate key (code 11000)".into()))
        );
        assert_eq!(failing_row("connection reset"), None);
    }

    #[test]
    fn only_real_ejson_wrappers_and_safe_numbers_are_typed() {
        let leaf = |t: &str| {
            let mut out = String::new();
            text_leaf(t, &mut out);
            out
        };
        assert_eq!(
            leaf(r#"{"$oid":"507f1f77bcf86cd799439011"}"#),
            r#"{"$oid":"507f1f77bcf86cd799439011"}"#
        );
        assert_eq!(
            leaf(r#"{"$numberDecimal":"1.5"}"#),
            r#"{"$numberDecimal":"1.5"}"#
        );
        assert_eq!(
            leaf(r#"{"$date":"2020-01-01T00:00:00Z"}"#),
            r#"{"$date":"2020-01-01T00:00:00Z"}"#
        );
        // Objects and arrays built from plain JSON and wrappers become values; operators nested
        // anywhere do not.
        assert_eq!(
            leaf(r#"[1,{"$oid":"507f1f77bcf86cd799439011"}]"#),
            r#"[1,{"$oid":"507f1f77bcf86cd799439011"}]"#
        );
        assert_eq!(leaf(r#"{"city":"Hue"}"#), r#"{"city":"Hue"}"#);
        assert_eq!(leaf(r#"{"a":{"$gt":1}}"#), r#""{\"a\":{\"$gt\":1}}""#);
        // Operators and unknown `$` keys are plain strings.
        assert_eq!(leaf(r#"{"$ne": 1}"#), r#""{\"$ne\": 1}""#);
        assert_eq!(leaf(r#"{"$where":"x"}"#), r#""{\"$where\":\"x\"}""#);
        // Integers beyond 64 bits keep every digit as text.
        assert_eq!(leaf("9223372036854775807"), "9223372036854775807");
        assert_eq!(leaf("18446744073709551616"), r#""18446744073709551616""#);
        assert_eq!(leaf("-9223372036854775809"), r#""-9223372036854775809""#);
        assert_eq!(leaf("1.5e3"), "1.5e3");
    }
}
