//! Import of CSV / XLSX / JSON files into a table, and of SQL scripts.

use std::path::PathBuf;
use std::sync::Arc;

use dbstudio_core::{
    BulkInsertRequest, CancelToken, ColumnMeta, EditValue, EndOutcome, Error, JobProgress,
    OnConflict, QueryRequest, Result, ResultSink, ScriptDialect, StatementEnd, TxOptions, Value,
};
use dbstudio_sql::Dialect;

use crate::conn::Conn;
use crate::writers::qualifier;
use crate::{
    ColumnMapping, ExportContext, ImportFormat, ImportMode, ImportPreview, ImportPreviewRequest,
    ImportReport, ImportSpec, ImportTarget, RowError, Transform,
};

mod insert;
mod preview;
mod script;
mod source;

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

/// Counts the rows a statement affected.
#[derive(Default)]
struct AffectedSink {
    affected: u64,
}

impl ResultSink for AffectedSink {
    fn on_columns(&mut self, _columns: Vec<ColumnMeta>) -> Result<()> {
        Ok(())
    }
    fn on_rows(&mut self, _rows: Vec<Vec<Value>>) -> Result<()> {
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
    report: ImportReport,
    progress: JobProgress,
}

impl Importer<'_> {
    fn has_lease(&self) -> bool {
        self.conn.lease().is_some()
    }

    async fn run_sql(&self, sql: String) -> Result<u64> {
        let mut sink = AffectedSink::default();
        self.conn
            .execute_cancellable(self.cancel, QueryRequest::new(sql), &mut sink)
            .await?;
        Ok(sink.affected)
    }

    async fn begin(&self) -> Result<()> {
        match self.conn.lease() {
            Some(l) => l.begin(TxOptions::default()).await,
            None => Ok(()),
        }
    }

    async fn commit(&self) -> Result<()> {
        match self.conn.lease() {
            Some(l) => l.commit().await,
            None => Ok(()),
        }
    }

    async fn rollback(&self) {
        if let Some(l) = self.conn.lease() {
            let _ = l.rollback().await;
        }
    }

    /// Insert `rows` (one transaction scope with the caller): bulk insert when the lease has
    /// one, otherwise multi-row `INSERT` statements.
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
        let mut counts = Counts::default();
        for chunk in rows.chunks(MAX_STATEMENT_ROWS) {
            let sql = insert_statement(self.dialect, &self.target, chunk, self.on_conflict)?;
            let affected = self.run_sql(sql).await?;
            let n = chunk.len() as u64;
            match self.on_conflict {
                OnConflict::Skip => {
                    counts.inserted += affected.min(n);
                    counts.skipped += n.saturating_sub(affected);
                }
                _ => counts.inserted += affected.min(n.max(affected)),
            }
        }
        Ok(counts)
    }

    fn add(&mut self, c: Counts) {
        self.report.inserted += c.inserted;
        self.report.updated += c.updated;
        self.report.skipped += c.skipped;
    }

    /// Insert one batch. A failing batch is undone and retried row by row to find the bad
    /// rows, which are reported with their line number.
    async fn load_batch(&mut self, batch: &[Mapped]) -> Result<()> {
        let rows: Vec<Vec<EditValue>> = batch.iter().map(|m| m.values.clone()).collect();
        let sp: Option<Savepoint> = if self.has_lease() {
            savepoint(self.dialect, "dbstudio_batch")
        } else {
            None
        };
        if let Some(sp) = &sp {
            self.run_sql(sp.create.clone()).await?;
        }
        let first = self.insert_rows(&rows).await;
        let error = match first {
            Ok(c) => {
                if let Some(sp) = &sp {
                    if let Some(release) = &sp.release {
                        self.run_sql(release.clone()).await?;
                    }
                }
                self.add(c);
                return Ok(());
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => e,
        };
        if self.has_lease() && sp.is_none() {
            // Nothing to roll back to: report the batch as a whole.
            let (a, b) = (
                batch.first().map_or(0, |m| m.line),
                batch.last().map_or(0, |m| m.line),
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
        for m in batch {
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
                    let message = e.message();
                    self.report.errors.push(RowError {
                        line: m.line,
                        message: message.clone(),
                    });
                    if self.report.errors.len() as u32 > self.max_errors {
                        return Err(Error::Query {
                            message: format!("line {}: {message}", m.line),
                            position: None,
                        });
                    }
                }
            }
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
    )
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
    let mut src = source::open(&path, &spec.format)?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let dialect = ctx.dialect;
    let q = Dialect::new(dialect);

    // Target table.
    let mut create_sql: Option<String> = None;
    let (qualified, table_columns, pk, table_path, display) = match &spec.target {
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
            let pk: Vec<String> = details
                .columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.clone())
                .collect();
            (qualified, cols, pk, Some(table.clone()), name)
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
            let pk: Vec<String> = columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.clone())
                .collect();
            (qualified, cols, pk, None, name.clone())
        }
        ImportTarget::Script { .. } => {
            return Err(Error::Other(
                "a table import needs an existing or a new table as target".into(),
            ));
        }
    };

    let mapping = resolve_mapping(&spec.mapping, src.columns(), &table_columns)?;
    let on_conflict = spec.on_conflict.unwrap_or(OnConflict::Error);
    let conflict_columns = pk;
    if on_conflict == OnConflict::Update && conflict_columns.is_empty() {
        return Err(Error::Other(
            "updating on conflict needs a table with a primary key".into(),
        ));
    }

    let (conn, _) = Conn::open(Arc::clone(&ctx.session), None).await?;
    let conn = Arc::new(conn);
    if matches!(spec.mode, ImportMode::SingleTransaction) && conn.lease().is_none() {
        return Err(Error::Unsupported("leases".into()));
    }
    let mut importer = Importer {
        ctx: &ctx,
        conn: Arc::clone(&conn),
        cancel: &cancel,
        dialect,
        target: Target {
            table: qualified,
            columns: mapping.iter().map(|m| m.target.clone()).collect(),
            conflict_columns,
        },
        table_path,
        on_conflict,
        max_errors: spec.max_errors,
        mode: spec.mode,
        bulk_supported: true,
        report: ImportReport::default(),
        progress: JobProgress {
            phase: "import".into(),
            object: Some(display),
            bytes_total: Some(size),
            ..JobProgress::default()
        },
    };
    let created = create_sql.is_some();
    let result = import_loop(&mut importer, &mut *src, &mapping, create_sql).await;
    if result.is_err() && created && matches!(spec.mode, ImportMode::SingleTransaction) {
        // All or nothing: do not leave the empty table the failed import created.
        let _ = importer
            .run_sql(format!("DROP TABLE {}", importer.target.table))
            .await;
    }
    let report = std::mem::take(&mut importer.report);
    drop(importer);
    if let Ok(conn) = Arc::try_unwrap(conn) {
        conn.close().await;
    }
    result.map(|()| report)
}

async fn import_loop(
    imp: &mut Importer<'_>,
    src: &mut dyn RowSource,
    mapping: &[MappedColumn],
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
    let outcome = load_all(imp, src, mapping, batch_rows, single).await;
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
            match src.next_row()? {
                Some(row) => {
                    let values = mapping
                        .iter()
                        .map(|m| {
                            let cell = row
                                .cells
                                .get(m.source_index)
                                .cloned()
                                .unwrap_or(EditValue::Value(Value::Null));
                            apply_transform(cell, m.transform)
                        })
                        .collect();
                    batch.push(Mapped {
                        line: row.line,
                        values,
                    });
                }
                None => {
                    eof = true;
                    break;
                }
            }
        }
        if batch.is_empty() {
            break;
        }
        imp.report.rows_read += batch.len() as u64;
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
}
