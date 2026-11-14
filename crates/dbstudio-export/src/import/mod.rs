//! Import of CSV / XLSX / JSON files into a table, and of SQL scripts.

use std::path::PathBuf;
use std::sync::Arc;

use dbstudio_core::{
    BulkInsertRequest, CancelToken, CollectingSink, ColumnDetail, ColumnMeta, EditValue,
    EndOutcome, Error, JobProgress, OnConflict, QueryRequest, Result, ResultSink, ScriptDialect,
    StatementEnd, StatementOutcome, TxOptions, Value, ValueKind,
};
use dbstudio_sql::Dialect;

use crate::conn::Conn;
use crate::writers::qualifier;
use crate::{
    ColumnMapping, ExportContext, ImportFormat, ImportMode, ImportPreview, ImportPreviewRequest,
    ImportReport, ImportSpec, ImportTarget, RowError, Transform,
};

mod document;
mod insert;
mod preview;
mod script;
pub(crate) mod source;

use insert::{MAX_STATEMENT_ROWS, Savepoint, Target, insert_statement, savepoint};
use source::RowSource;

/// Preview a file before importing.
pub async fn preview_import(req: ImportPreviewRequest) -> Result<ImportPreview> {
    preview::preview(req).await
}

/// Run an import.
pub async fn run_import(ctx: ExportContext, spec: ImportSpec) -> Result<ImportReport> {
    let cancel = ctx.reporter.cancel_token().clone();
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if matches!(spec.target, ImportTarget::Keys { .. }) {
        return crate::keys::import_keys(&ctx, &spec, &cancel).await;
    }
    if matches!(ctx.dialect, ScriptDialect::MongoShell)
        && !matches!(spec.format, ImportFormat::Sql { .. })
    {
        return document::run(&ctx, &spec, &cancel).await;
    }
    match &spec.format {
        ImportFormat::Sql { continue_on_error } => {
            script::run(&ctx, &spec, *continue_on_error, &cancel).await
        }
        _ => run_rows(ctx, spec, cancel).await,
    }
}

// ---------------------------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MappedColumn {
    pub source_index: usize,
    pub target: String,
    pub transform: Transform,
}

fn find_ci(names: &[String], wanted: &str) -> Option<usize> {
    names
        .iter()
        .position(|n| n == wanted)
        .or_else(|| names.iter().position(|n| n.eq_ignore_ascii_case(wanted)))
}

/// Resolve the user's mapping (or map by name when it is empty) against the file's columns and
/// the table's columns.
pub(crate) fn resolve_mapping(
    mapping: &[ColumnMapping],
    file_columns: &[String],
    table_columns: &[String],
) -> Result<Vec<MappedColumn>> {
    let mut out: Vec<MappedColumn> = Vec::new();
    if mapping.is_empty() {
        for (i, name) in file_columns.iter().enumerate() {
            if let Some(t) = find_ci(table_columns, name) {
                out.push(MappedColumn {
                    source_index: i,
                    target: table_columns[t].clone(),
                    transform: Transform::None,
                });
            }
        }
        if out.is_empty() {
            return Err(Error::Other(
                "no column of the file matches a column of the table".into(),
            ));
        }
        return Ok(out);
    }
    for m in mapping {
        let source_index = match m
            .source
            .strip_prefix('#')
            .and_then(|n| n.parse::<usize>().ok())
        {
            Some(n) if n >= 1 && n <= file_columns.len() => n - 1,
            Some(_) => {
                return Err(Error::Other(format!(
                    "column {} is outside the file's {} column(s)",
                    m.source,
                    file_columns.len()
                )));
            }
            None => find_ci(file_columns, &m.source).ok_or_else(|| {
                Error::Other(format!("column not found in the file: {}", m.source))
            })?,
        };
        let target = find_ci(table_columns, &m.target)
            .map(|t| table_columns[t].clone())
            .ok_or_else(|| Error::Other(format!("column not found in the table: {}", m.target)))?;
        if out.iter().any(|o| o.target == target) {
            return Err(Error::Other(format!(
                "column {target} is mapped more than once"
            )));
        }
        out.push(MappedColumn {
            source_index,
            target,
            transform: m.transform,
        });
    }
    Ok(out)
}

fn apply_transform(cell: EditValue, t: Transform) -> EditValue {
    let EditValue::Text(s) = cell else {
        return cell;
    };
    let s = if matches!(t, Transform::Trim | Transform::TrimEmptyAsNull) {
        s.trim().to_string()
    } else {
        s
    };
    if matches!(t, Transform::EmptyAsNull | Transform::TrimEmptyAsNull) && s.is_empty() {
        return EditValue::Value(Value::Null);
    }
    EditValue::Text(s)
}

// ---------------------------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------------------------

/// Counts the rows a statement affected, and the rows of a `RETURNING (xmax = 0)` result
/// (`true` = inserted, `false` = updated).
#[derive(Default)]
struct AffectedSink {
    affected: u64,
    returned: u64,
    returned_inserted: u64,
}

impl ResultSink for AffectedSink {
    fn on_columns(&mut self, _columns: Vec<ColumnMeta>) -> Result<()> {
        Ok(())
    }
    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        for r in &rows {
            self.returned += 1;
            if matches!(r.first(), Some(Value::Bool(true))) {
                self.returned_inserted += 1;
            }
        }
        Ok(())
    }
    fn on_message(&mut self, _message: String) -> Result<()> {
        Ok(())
    }
    fn on_statement_end(&mut self, end: StatementEnd) -> Result<()> {
        if let EndOutcome::Affected { rows } = end.outcome {
            self.affected += rows;
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    inserted: u64,
    updated: u64,
    skipped: u64,
}

struct Mapped {
    line: u64,
    values: Vec<EditValue>,
}

/// Default size cap of one `INSERT` statement (the MySQL packet limit lowers it).
const DEFAULT_STATEMENT_BYTES: usize = 1024 * 1024;

/// Rough size of the SQL text of a row.
fn row_bytes(row: &[EditValue]) -> usize {
    row.iter()
        .map(|c| match c {
            EditValue::Text(s) => s.len() + 4,
            EditValue::Default => 8,
            EditValue::Value(v) => match v {
                Value::Text(s)
                | Value::Decimal(s)
                | Value::Date(s)
                | Value::Time(s)
                | Value::DateTime(s)
                | Value::Timestamp(s)
                | Value::Uuid(s)
                | Value::Json(s) => s.len() + 4,
                Value::Bytes(b) => b.len() * 2 + 6,
                _ => 24,
            },
        })
        .sum::<usize>()
        + 4
}

/// Split `rows` into statements of at most [`MAX_STATEMENT_ROWS`] rows and `max_bytes` bytes
/// (a single big row still gets a statement of its own).
fn statement_ranges(rows: &[Mapped], max_bytes: usize) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let (mut start, mut bytes) = (0usize, 0usize);
    for (i, m) in rows.iter().enumerate() {
        let b = row_bytes(&m.values);
        if i > start && (i - start >= MAX_STATEMENT_ROWS || bytes + b > max_bytes) {
            out.push(start..i);
            start = i;
            bytes = 0;
        }
        bytes += b;
    }
    if start < rows.len() {
        out.push(start..rows.len());
    }
    out
}

struct Importer<'a> {
    ctx: &'a ExportContext,
    conn: Arc<Conn>,
    cancel: &'a CancelToken,
    dialect: ScriptDialect,
    target: Target,
    /// Path of an existing table, for `Lease::bulk_insert`.
    table_path: Option<dbstudio_core::ObjectPath>,
    on_conflict: OnConflict,
    max_errors: u32,
    mode: ImportMode,
    bulk_supported: bool,
    max_statement_bytes: usize,
    report: ImportReport,
    progress: JobProgress,
}

impl Importer<'_> {
    fn has_lease(&self) -> bool {
        self.conn.lease().is_some()
    }

    async fn run_sink(&self, sql: String, sink: &mut AffectedSink) -> Result<()> {
        self.conn
            .execute_cancellable(self.cancel, QueryRequest::new(sql), sink)
            .await
            .map(|_| ())
    }

    async fn run_sql(&self, sql: String) -> Result<u64> {
        let mut sink = AffectedSink::default();
        self.run_sink(sql, &mut sink).await?;
        Ok(sink.affected)
    }

    /// Statements run inside a transaction of the lease (false: autocommit dialects).
    fn in_tx(&self) -> bool {
        self.has_lease() && has_transactions(self.dialect)
    }

    async fn begin(&self) -> Result<()> {
        if !has_transactions(self.dialect) {
            return Ok(());
        }
        match self.conn.lease() {
            Some(l) => l.begin(TxOptions::default()).await,
            None => Ok(()),
        }
    }

    async fn commit(&self) -> Result<()> {
        if !has_transactions(self.dialect) {
            return Ok(());
        }
        match self.conn.lease() {
            Some(l) => l.commit().await,
            None => Ok(()),
        }
    }

    async fn rollback(&self) {
        if !has_transactions(self.dialect) {
            return;
        }
        if let Some(l) = self.conn.lease() {
            let _ = l.rollback().await;
        }
    }

    /// Insert `rows` (one statement's worth, inside the caller's transaction scope): the
    /// lease's bulk insert (same connection, same transaction) when it has one, otherwise one
    /// multi-row `INSERT` statement.
    async fn insert_rows(&mut self, rows: &[Vec<EditValue>]) -> Result<Counts> {
        if self.bulk_supported {
            if let (Some(lease), Some(table)) = (self.conn.lease(), &self.table_path) {
                let req = BulkInsertRequest {
                    table: table.clone(),
                    columns: self.target.columns.clone(),
                    rows: rows.to_vec(),
                    on_conflict: self.on_conflict,
                    conflict_columns: self.target.conflict_columns.clone(),
                };
                match lease.bulk_insert(req).await {
                    Ok(r) => {
                        return Ok(Counts {
                            inserted: r.inserted,
                            updated: r.updated,
                            skipped: r.skipped,
                        });
                    }
                    Err(Error::Unsupported(_)) => self.bulk_supported = false,
                    Err(e) => return Err(map_cancel(e, self.cancel)),
                }
            } else {
                self.bulk_supported = false;
            }
        }
        if matches!(self.dialect, ScriptDialect::Cql) {
            return self.insert_rows_cql(rows).await;
        }
        let n = rows.len() as u64;
        let returning = matches!(self.dialect, ScriptDialect::Postgres)
            && self.on_conflict == OnConflict::Update;
        let sql = insert_statement(
            self.dialect,
            &self.target,
            rows,
            self.on_conflict,
            returning,
        )?;
        let mut sink = AffectedSink::default();
        self.run_sink(sql, &mut sink).await?;
        let affected = sink.affected;
        Ok(match self.on_conflict {
            OnConflict::Error => Counts {
                inserted: affected.min(n),
                ..Counts::default()
            },
            OnConflict::Skip => Counts {
                inserted: affected.min(n),
                skipped: n.saturating_sub(affected),
                updated: 0,
            },
            OnConflict::Update if returning => Counts {
                inserted: sink.returned_inserted,
                updated: sink.returned - sink.returned_inserted,
                skipped: n.saturating_sub(sink.returned),
            },
            OnConflict::Update => {
                // MySQL counts 1 per inserted and 2 per changed row.
                let (inserted, updated) = if affected > n {
                    (n.saturating_sub(affected - n), affected - n)
                } else {
                    (affected, 0)
                };
                Counts {
                    inserted,
                    updated,
                    skipped: 0,
                }
            }
        })
    }

    /// CQL has no multi-row `INSERT`: one statement per row. With `Skip` the lightweight
    /// transaction answers `[applied]` per row.
    async fn insert_rows_cql(&mut self, rows: &[Vec<EditValue>]) -> Result<Counts> {
        let mut counts = Counts::default();
        for row in rows {
            let sql = insert_statement(
                self.dialect,
                &self.target,
                std::slice::from_ref(row),
                self.on_conflict,
                false,
            )?;
            let mut sink = AffectedSink::default();
            self.run_sink(sql, &mut sink).await?;
            if self.on_conflict == OnConflict::Skip && sink.returned > 0 {
                if sink.returned_inserted > 0 {
                    counts.inserted += 1;
                } else {
                    counts.skipped += 1;
                }
            } else {
                counts.inserted += 1;
            }
        }
        Ok(counts)
    }

    fn add(&mut self, c: Counts) {
        self.report.inserted += c.inserted;
        self.report.updated += c.updated;
        self.report.skipped += c.skipped;
    }

    /// Insert one batch, statement by statement. Only the statement that fails is undone and
    /// retried row by row, so rows of the statements before it are never inserted twice.
    async fn load_batch(&mut self, batch: &[Mapped]) -> Result<()> {
        for range in statement_ranges(batch, self.max_statement_bytes) {
            self.load_chunk(&batch[range]).await?;
        }
        Ok(())
    }

    /// One statement's rows. With a lease a savepoint makes the failed statement undoable inside
    /// the open transaction; without one the statement is atomic by itself.
    async fn load_chunk(&mut self, chunk: &[Mapped]) -> Result<()> {
        let rows: Vec<Vec<EditValue>> = chunk.iter().map(|m| m.values.clone()).collect();
        let sp: Option<Savepoint> = if self.has_lease() {
            savepoint(self.dialect, "dbstudio_chunk")
        } else {
            None
        };
        if let Some(sp) = &sp {
            self.run_sql(sp.create.clone()).await?;
        }
        let error = match self.insert_rows(&rows).await {
            Ok(c) => {
                if let Some(release) = sp.as_ref().and_then(|s| s.release.clone()) {
                    self.run_sql(release).await?;
                }
                self.add(c);
                return Ok(());
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => e,
        };
        if self.in_tx() && sp.is_none() {
            // Nothing to roll back to: report the statement as a whole.
            let (a, b) = (
                chunk.first().map_or(0, |m| m.line),
                chunk.last().map_or(0, |m| m.line),
            );
            return Err(Error::Query {
                message: format!("lines {a}-{b}: {}", error.message()),
                position: None,
            });
        }
        if let Some(sp) = &sp {
            self.run_sql(sp.rollback.clone()).await?;
        }
        // Row by row, each in its own savepoint.
        let row_sp = if self.has_lease() {
            savepoint(self.dialect, "dbstudio_row")
        } else {
            None
        };
        for m in chunk {
            if self.cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if let Some(rsp) = &row_sp {
                self.run_sql(rsp.create.clone()).await?;
            }
            let one = [m.values.clone()];
            match self.insert_rows(&one).await {
                Ok(c) => {
                    if let Some(release) = row_sp.as_ref().and_then(|s| s.release.clone()) {
                        self.run_sql(release).await?;
                    }
                    self.add(c);
                }
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => {
                    if let Some(rsp) = &row_sp {
                        self.run_sql(rsp.rollback.clone()).await?;
                    }
                    self.row_failed(m.line, e.message())?;
                }
            }
        }
        Ok(())
    }

    /// Record a failed row; stops the import when more rows failed than tolerated.
    fn row_failed(&mut self, line: u64, message: String) -> Result<()> {
        self.report.errors.push(RowError {
            line,
            message: message.clone(),
        });
        if self.report.errors.len() as u32 > self.max_errors {
            return Err(Error::Query {
                message: format!("line {line}: {message}"),
                position: None,
            });
        }
        Ok(())
    }

    fn report_progress(&mut self, bytes: u64) {
        self.progress.rows_done = self.report.rows_read;
        self.progress.bytes_done = bytes;
        self.ctx.reporter.progress(&self.progress);
    }
}

fn map_cancel(e: Error, cancel: &CancelToken) -> Error {
    if cancel.is_cancelled() {
        Error::Cancelled
    } else {
        e
    }
}

fn is_sql_dialect(d: ScriptDialect) -> bool {
    matches!(
        d,
        ScriptDialect::Postgres
            | ScriptDialect::MySql { .. }
            | ScriptDialect::MsSql
            | ScriptDialect::Oracle
            | ScriptDialect::Cql
            | ScriptDialect::ClickHouse
    )
}

/// Dialects whose servers have no multi-statement transactions (CQL, ClickHouse): the import
/// commits as it goes and cannot undo a failed statement.
fn has_transactions(d: ScriptDialect) -> bool {
    !matches!(d, ScriptDialect::Cql | ScriptDialect::ClickHouse)
}

/// How a target column wants its cells coerced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColKind {
    Bytes,
    Bool,
    Plain,
}

fn kind_of_detail(c: &ColumnDetail) -> ColKind {
    match c.value_kind {
        ValueKind::Bytes => ColKind::Bytes,
        ValueKind::Bool => ColKind::Bool,
        _ if c.type_name.to_lowercase().replace(' ', "") == "tinyint(1)" => ColKind::Bool,
        _ => ColKind::Plain,
    }
}

/// Kind of a column that is about to be created, from its type name.
fn kind_of_type_name(type_name: &str) -> ColKind {
    let t = type_name.to_lowercase();
    if ["bytea", "blob", "binary", "raw"]
        .iter()
        .any(|k| t.contains(k))
    {
        ColKind::Bytes
    } else if t.contains("bool") || t.trim() == "bit" {
        ColKind::Bool
    } else {
        ColKind::Plain
    }
}

/// Bytes of a text cell: base64 (what the exports write), or hex with a `0x` / `\x` prefix.
fn decode_binary(text: &str) -> std::result::Result<Vec<u8>, String> {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
    let t = text.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("\\x")) {
        if hex.len() % 2 != 0 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("invalid hexadecimal binary value".into());
        }
        return Ok((0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0))
            .collect());
    }
    STANDARD
        .decode(t)
        .or_else(|_| STANDARD_NO_PAD.decode(t))
        .map_err(|_| "invalid base64 binary value".to_string())
}

/// Coerce a cell to what its (binary / boolean) target column needs. Other columns get the
/// text as typed: the database converts it to the column type.
fn coerce(cell: EditValue, kind: ColKind) -> std::result::Result<EditValue, String> {
    match (kind, cell) {
        (ColKind::Bytes, EditValue::Text(s)) => {
            decode_binary(&s).map(|b| EditValue::Value(Value::Bytes(b)))
        }
        (ColKind::Bool, EditValue::Text(s)) => Ok(match s.trim().to_ascii_lowercase().as_str() {
            "true" | "t" => EditValue::Value(Value::Bool(true)),
            "false" | "f" => EditValue::Value(Value::Bool(false)),
            _ => EditValue::Text(s),
        }),
        (_, other) => Ok(other),
    }
}

/// Packet limit of a MySQL server bounds the size of an `INSERT`.
async fn statement_limit(conn: &Arc<Conn>, dialect: ScriptDialect) -> usize {
    if !matches!(dialect, ScriptDialect::MySql { .. }) {
        return DEFAULT_STATEMENT_BYTES;
    }
    let mut sink = CollectingSink::new();
    let packet = match conn
        .execute(QueryRequest::new("SELECT @@max_allowed_packet"), &mut sink)
        .await
    {
        Ok(_) => sink
            .into_results()
            .into_iter()
            .next()
            .and_then(|r| match r.outcome {
                StatementOutcome::Rows(rs) => rs.rows.first().and_then(|row| match row.first() {
                    Some(Value::Int(i)) => u64::try_from(*i).ok(),
                    Some(Value::UInt(u)) => Some(*u),
                    Some(Value::Text(t) | Value::Decimal(t)) => t.parse().ok(),
                    Some(Value::Bytes(b)) => String::from_utf8_lossy(b).parse().ok(),
                    _ => None,
                }),
                _ => None,
            }),
        Err(_) => None,
    };
    match packet {
        // Leave room for the escaping and the statement head.
        Some(p) => DEFAULT_STATEMENT_BYTES.min((p / 4) as usize).max(4096),
        None => DEFAULT_STATEMENT_BYTES,
    }
}

async fn run_rows(
    ctx: ExportContext,
    spec: ImportSpec,
    cancel: CancelToken,
) -> Result<ImportReport> {
    if !is_sql_dialect(ctx.dialect) {
        return Err(Error::Unsupported("import_dialect".into()));
    }
    let path = PathBuf::from(&spec.file);
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    // Opening a workbook or scanning a JSON file for its keys must not block the runtime.
    let (open_path, open_format) = (path.clone(), spec.format.clone());
    let mut src =
        tokio::task::spawn_blocking(move || source::open(&open_path, &open_format, false))
            .await
            .map_err(|e| Error::Other(format!("import task failed: {e}")))??;
    let dialect = ctx.dialect;
    let q = Dialect::new(dialect);

    // Target table.
    let mut create_sql: Option<String> = None;
    let (qualified, table_columns, kinds, pk, table_path, display, database) = match &spec.target {
        ImportTarget::Existing { table } => {
            let details = ctx.session.table_details(table).await?;
            let name = table
                .last()
                .map(|s| s.name.clone())
                .ok_or_else(|| Error::Other("invalid table path".into()))?;
            let qualified = match qualifier(Some(table), None, dialect) {
                Some(s) => format!("{}.{}", q.quote_ident(&s), q.quote_ident(&name)),
                None => q.quote_ident(&name),
            };
            let cols: Vec<String> = details.columns.iter().map(|c| c.name.clone()).collect();
            let kinds: Vec<ColKind> = details.columns.iter().map(kind_of_detail).collect();
            let pk: Vec<String> = details
                .columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.clone())
                .collect();
            (
                qualified,
                cols,
                kinds,
                pk,
                Some(table.clone()),
                name,
                crate::conn::database_of(table),
            )
        }
        ImportTarget::New {
            parent,
            name,
            columns,
        } => {
            if columns.is_empty() {
                return Err(Error::Other("the new table has no columns".into()));
            }
            let qualified = match qualifier(Some(parent), None, dialect) {
                Some(s) => format!("{}.{}", q.quote_ident(&s), q.quote_ident(name)),
                None => q.quote_ident(name),
            };
            let plain = dbstudio_sql::ddl::create_table(dialect, name, columns);
            let head = format!("CREATE TABLE {}", q.quote_ident(name));
            let sql = match plain.strip_prefix(&head) {
                Some(rest) => format!("CREATE TABLE {qualified}{rest}"),
                None => plain,
            };
            create_sql = Some(sql);
            let cols: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
            let kinds: Vec<ColKind> = columns
                .iter()
                .map(|c| kind_of_type_name(&c.type_name))
                .collect();
            let pk: Vec<String> = columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.clone())
                .collect();
            (
                qualified,
                cols,
                kinds,
                pk,
                None,
                name.clone(),
                crate::conn::database_of(parent),
            )
        }
        ImportTarget::Script { .. } => {
            return Err(Error::Invalid(
                "a table import needs an existing or a new table as target".into(),
            ));
        }
        // Redis keys are imported by `crate::keys` (see `run_import`).
        ImportTarget::Keys { .. } => {
            return Err(Error::Invalid(
                "a table import needs an existing or a new table as target".into(),
            ));
        }
    };

    let mapping = resolve_mapping(&spec.mapping, src.columns(), &table_columns)?;
    let col_kinds: Vec<ColKind> = mapping
        .iter()
        .map(|m| {
            table_columns
                .iter()
                .position(|c| *c == m.target)
                .and_then(|i| kinds.get(i).copied())
                .unwrap_or(ColKind::Plain)
        })
        .collect();
    let on_conflict = spec.on_conflict.unwrap_or(OnConflict::Error);
    let conflict_columns = pk;
    if on_conflict == OnConflict::Update && conflict_columns.is_empty() {
        return Err(Error::Other(
            "updating on conflict needs a table with a primary key".into(),
        ));
    }

    let (conn, _) = Conn::open(Arc::clone(&ctx.session), database, None).await?;
    let conn = Arc::new(conn);
    let result = import_table(
        &ctx,
        &cancel,
        &conn,
        &spec,
        &mut *src,
        TableTarget {
            qualified,
            table_path,
            display,
            conflict_columns,
            on_conflict,
            create_sql,
            mapping,
            col_kinds,
            size,
        },
    )
    .await;
    conn.close().await;
    result
}

/// Everything `import_table` needs about the table.
struct TableTarget {
    qualified: String,
    table_path: Option<dbstudio_core::ObjectPath>,
    display: String,
    conflict_columns: Vec<String>,
    on_conflict: OnConflict,
    create_sql: Option<String>,
    mapping: Vec<MappedColumn>,
    col_kinds: Vec<ColKind>,
    size: u64,
}

async fn import_table(
    ctx: &ExportContext,
    cancel: &CancelToken,
    conn: &Arc<Conn>,
    spec: &ImportSpec,
    src: &mut dyn RowSource,
    t: TableTarget,
) -> Result<ImportReport> {
    if matches!(spec.mode, ImportMode::SingleTransaction)
        && has_transactions(ctx.dialect)
        && conn.lease().is_none()
    {
        return Err(Error::Unsupported("leases".into()));
    }
    let dialect = ctx.dialect;
    let mut importer = Importer {
        ctx,
        conn: Arc::clone(conn),
        cancel,
        dialect,
        target: Target {
            table: t.qualified,
            columns: t.mapping.iter().map(|m| m.target.clone()).collect(),
            conflict_columns: t.conflict_columns,
        },
        table_path: t.table_path,
        on_conflict: t.on_conflict,
        max_errors: spec.max_errors,
        mode: spec.mode,
        bulk_supported: true,
        max_statement_bytes: statement_limit(conn, dialect).await,
        report: ImportReport::default(),
        progress: JobProgress {
            phase: "import".into(),
            object: Some(t.display),
            bytes_total: Some(t.size),
            ..JobProgress::default()
        },
    };
    let created = t.create_sql.is_some();
    let result = import_loop(&mut importer, src, &t.mapping, &t.col_kinds, t.create_sql).await;
    if result.is_err() && created && matches!(spec.mode, ImportMode::SingleTransaction) {
        // All or nothing: do not leave the empty table the failed import created.
        let _ = importer
            .run_sql(format!("DROP TABLE {}", importer.target.table))
            .await;
    }
    let report = std::mem::take(&mut importer.report);
    result.map(|()| report)
}

async fn import_loop(
    imp: &mut Importer<'_>,
    src: &mut dyn RowSource,
    mapping: &[MappedColumn],
    kinds: &[ColKind],
    create_sql: Option<String>,
) -> Result<()> {
    if let Some(sql) = create_sql {
        imp.run_sql(sql).await?;
    }
    let batch_rows = match imp.mode {
        ImportMode::SingleTransaction => MAX_STATEMENT_ROWS,
        ImportMode::Batches { rows } => rows.max(1) as usize,
    };
    let single = matches!(imp.mode, ImportMode::SingleTransaction);
    if single {
        imp.begin().await?;
    }
    let outcome = load_all(imp, src, mapping, kinds, batch_rows, single).await;
    match outcome {
        Ok(()) => {
            if single {
                imp.commit().await?;
            }
            let bytes = src.bytes_read();
            imp.report_progress(bytes);
            Ok(())
        }
        Err(e) => {
            if single {
                imp.rollback().await;
            }
            Err(e)
        }
    }
}

async fn load_all(
    imp: &mut Importer<'_>,
    src: &mut dyn RowSource,
    mapping: &[MappedColumn],
    kinds: &[ColKind],
    batch_rows: usize,
    single: bool,
) -> Result<()> {
    let mut eof = false;
    while !eof {
        if imp.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut batch: Vec<Mapped> = Vec::with_capacity(batch_rows.min(10_000));
        while batch.len() < batch_rows {
            let Some(row) = src.next_row()? else {
                eof = true;
                break;
            };
            imp.report.rows_read += 1;
            let mut values = Vec::with_capacity(mapping.len());
            let mut bad: Option<String> = None;
            for (m, kind) in mapping.iter().zip(kinds) {
                let cell = row
                    .cells
                    .get(m.source_index)
                    .cloned()
                    .unwrap_or(EditValue::Value(Value::Null));
                match coerce(apply_transform(cell, m.transform), *kind) {
                    Ok(v) => values.push(v),
                    Err(e) => {
                        bad = Some(format!("column {}: {e}", m.target));
                        break;
                    }
                }
            }
            match bad {
                Some(message) => imp.row_failed(row.line, message)?,
                None => batch.push(Mapped {
                    line: row.line,
                    values,
                }),
            }
        }
        if batch.is_empty() {
            continue;
        }
        if !single {
            imp.begin().await?;
        }
        match imp.load_batch(&batch).await {
            Ok(()) => {
                if !single {
                    imp.commit().await?;
                }
            }
            Err(e) => {
                if !single {
                    imp.rollback().await;
                }
                return Err(e);
            }
        }
        let bytes = src.bytes_read();
        imp.report_progress(bytes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn map(source: &str, target: &str, transform: Transform) -> ColumnMapping {
        ColumnMapping {
            source: source.into(),
            target: target.into(),
            transform,
        }
    }

    #[test]
    fn auto_mapping_by_name() {
        let r = resolve_mapping(
            &[],
            &names(&["ID", "extra", "Name"]),
            &names(&["id", "name"]),
        )
        .unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].source_index, r[0].target.as_str()), (0, "id"));
        assert_eq!((r[1].source_index, r[1].target.as_str()), (2, "name"));
        assert!(resolve_mapping(&[], &names(&["x"]), &names(&["id"])).is_err());
    }

    #[test]
    fn explicit_mapping_and_errors() {
        let file = names(&["a", "b"]);
        let table = names(&["id", "label"]);
        let r = resolve_mapping(
            &[
                map("#2", "ID", Transform::Trim),
                map("a", "label", Transform::None),
            ],
            &file,
            &table,
        )
        .unwrap();
        assert_eq!(r[0].source_index, 1);
        assert_eq!(r[0].target, "id");
        assert_eq!(r[0].transform, Transform::Trim);
        assert_eq!(r[1].source_index, 0);
        assert!(resolve_mapping(&[map("#3", "id", Transform::None)], &file, &table).is_err());
        assert!(resolve_mapping(&[map("zz", "id", Transform::None)], &file, &table).is_err());
        assert!(resolve_mapping(&[map("a", "nope", Transform::None)], &file, &table).is_err());
        assert!(
            resolve_mapping(
                &[
                    map("a", "id", Transform::None),
                    map("b", "id", Transform::None)
                ],
                &file,
                &table
            )
            .is_err()
        );
    }

    #[test]
    fn transforms() {
        let t = |s: &str, tr| apply_transform(EditValue::Text(s.into()), tr);
        assert_eq!(t("  x ", Transform::Trim), EditValue::Text("x".into()));
        assert_eq!(t("  x ", Transform::None), EditValue::Text("  x ".into()));
        assert_eq!(t("", Transform::EmptyAsNull), EditValue::Value(Value::Null));
        assert_eq!(
            t("  ", Transform::EmptyAsNull),
            EditValue::Text("  ".into())
        );
        assert_eq!(
            t("  ", Transform::TrimEmptyAsNull),
            EditValue::Value(Value::Null)
        );
        assert_eq!(
            apply_transform(EditValue::Value(Value::Int(1)), Transform::Trim),
            EditValue::Value(Value::Int(1))
        );
    }

    #[test]
    fn binary_cells_are_decoded_for_binary_columns_only() {
        assert_eq!(decode_binary("AQID").unwrap(), [1, 2, 3]);
        assert_eq!(decode_binary("AQI").unwrap(), [1, 2]);
        assert_eq!(decode_binary("0xdeAD").unwrap(), [0xde, 0xad]);
        assert_eq!(decode_binary("\\x01ff").unwrap(), [1, 255]);
        assert!(decode_binary("not base64!").is_err());
        assert!(decode_binary("0xabc").is_err());
        let text = || EditValue::Text("AQID".into());
        assert_eq!(
            coerce(text(), ColKind::Bytes).unwrap(),
            EditValue::Value(Value::Bytes(vec![1, 2, 3]))
        );
        assert_eq!(coerce(text(), ColKind::Plain).unwrap(), text());
        assert!(coerce(EditValue::Text("%%".into()), ColKind::Bytes).is_err());
        assert_eq!(
            coerce(EditValue::Value(Value::Null), ColKind::Bytes).unwrap(),
            EditValue::Value(Value::Null)
        );
        assert_eq!(
            coerce(EditValue::Text("TRUE".into()), ColKind::Bool).unwrap(),
            EditValue::Value(Value::Bool(true))
        );
        assert_eq!(
            coerce(EditValue::Text("maybe".into()), ColKind::Bool).unwrap(),
            EditValue::Text("maybe".into())
        );
        assert_eq!(kind_of_type_name("bytea"), ColKind::Bytes);
        assert_eq!(kind_of_type_name("LONGBLOB"), ColKind::Bytes);
        assert_eq!(kind_of_type_name("boolean"), ColKind::Bool);
        assert_eq!(kind_of_type_name("text"), ColKind::Plain);
    }

    #[test]
    fn statements_respect_row_and_byte_limits() {
        let row = |n: usize| Mapped {
            line: 1,
            values: vec![EditValue::Text("x".repeat(n))],
        };
        let rows: Vec<Mapped> = (0..2500).map(|_| row(10)).collect();
        let ranges = statement_ranges(&rows, 1 << 20);
        assert_eq!(ranges, vec![0..1000, 1000..2000, 2000..2500]);
        // Bytes: 100 KiB rows, 250 KiB per statement.
        let big: Vec<Mapped> = (0..5).map(|_| row(100 * 1024)).collect();
        assert_eq!(statement_ranges(&big, 250 * 1024), vec![0..2, 2..4, 4..5]);
        // One row bigger than the limit still gets a statement.
        assert_eq!(statement_ranges(&[row(10_000)], 100), vec![0..1]);
        assert!(statement_ranges(&[], 100).is_empty());
    }
}
