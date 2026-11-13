//! The export engine: resolves the objects, streams rows into a format writer, reports progress
//! and honours cancellation. Nothing is left on disk unless the export completes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dbstudio_core::{
    CancelToken, ColumnDetail, ColumnMeta, DdlItem, DdlOptions, DdlRequest, DdlSection, EndOutcome,
    Error, IsolationLevel, JobProgress, JobReporter, ObjectKind, ObjectPath, QueryRequest, Result,
    ResultSink, SchemaModelRequest, ScriptDialect, StatementEnd, TableDataRequest, TableModel,
    TableStreamRequest, TxOptions, Value,
};
use dbstudio_sql::Dialect;
use uuid::Uuid;

use crate::conn::Conn;
use crate::writers::{WriterConfig, create_writer, qualifier};
use crate::{
    ExportContent, ExportContext, ExportFormat, ExportReport, ExportSource, ExportSpec,
    ExportTuning, ObjectMeta, TableWriter,
};

/// Rows per page when a table is read through `fetch_table` (document / key-value drivers).
const PAGE_ROWS: u64 = 50_000;

/// Run an export.
pub async fn run_export(ctx: ExportContext, spec: ExportSpec) -> Result<ExportReport> {
    run_export_tuned(ctx, spec, ExportTuning::default()).await
}

/// [`run_export`] with adjustable limits (tests lower the XLSX sheet size).
pub async fn run_export_tuned(
    ctx: ExportContext,
    spec: ExportSpec,
    tuning: ExportTuning,
) -> Result<ExportReport> {
    let cancel = ctx.reporter.cancel_token().clone();
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if matches!(spec.source, ExportSource::Keys { .. }) {
        return Err(Error::Unsupported("export_keys".into()));
    }
    let mut warnings = Vec::new();

    let work = plan(&ctx, &spec, &mut warnings).await?;
    check_destination(&spec, &work)?;

    // One snapshot for the whole export: a read-only repeatable-read transaction on a lease.
    let tx = TxOptions {
        read_only: true,
        isolation: Some(IsolationLevel::RepeatableRead),
    };
    let (conn, note) =
        Conn::open(Arc::clone(&ctx.session), work.database.clone(), Some(tx)).await?;
    warnings.extend(note);
    let conn = Arc::new(conn);

    let result = run(&ctx, &spec, &tuning, &work, &conn, &cancel, &mut warnings).await;
    conn.close().await;
    result
}

// ---------------------------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------------------------

/// What an export will write.
#[derive(Debug)]
struct Work {
    /// Tables in export order; empty for a query.
    tables: Vec<TableModel>,
    /// Scopes to ask `generate_ddl` about: `(scope, restrict-to objects)`.
    ddl_scopes: Vec<(ObjectPath, Vec<ObjectPath>)>,
    /// Qualifiers differ between tables: entry names carry the schema.
    multi_schema: bool,
    /// Database every table lives in (one connection reads them all).
    database: Option<String>,
    /// Number of files the output holds.
    files: usize,
    content: ExportContent,
}

fn is_data_object(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Table
            | ObjectKind::View
            | ObjectKind::MaterializedView
            | ObjectKind::Collection
    )
}

/// The schema / database node a table path belongs to (the path without trailing `Table` and
/// `Folder` segments).
pub(crate) fn scope_of(path: &ObjectPath) -> ObjectPath {
    let mut segments = path.segments.clone();
    while matches!(
        segments.last().map(|s| s.kind),
        Some(ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView)
            | Some(ObjectKind::Collection)
            | Some(ObjectKind::Folder)
    ) {
        segments.pop();
    }
    ObjectPath { segments }
}

async fn plan(ctx: &ExportContext, spec: &ExportSpec, warnings: &mut Vec<String>) -> Result<Work> {
    let session = &ctx.session;
    let request = |scope: ObjectPath, tables: Vec<ObjectPath>, views: bool| SchemaModelRequest {
        scope,
        tables,
        include_views: views,
        include_comments: true,
        detail: dbstudio_core::ModelDetail::Full,
        sample_size: None,
    };
    let mut tables: Vec<TableModel> = Vec::new();
    let mut ddl_scopes: Vec<(ObjectPath, Vec<ObjectPath>)> = Vec::new();
    match &spec.source {
        ExportSource::Database { path } | ExportSource::Schema { path } => {
            let model = session
                .schema_model(request(path.clone(), Vec::new(), false))
                .await?;
            if model.truncated {
                warnings.push("the schema has more tables than the export lists".into());
            }
            tables.extend(model.tables.into_iter().filter(|t| is_data_object(t.kind)));
            ddl_scopes.push((path.clone(), Vec::new()));
        }
        ExportSource::Tables { paths } => {
            if paths.is_empty() {
                return Err(Error::Other("no tables selected".into()));
            }
            let mut groups: Vec<(ObjectPath, Vec<ObjectPath>)> = Vec::new();
            for p in paths {
                let scope = scope_of(p);
                match groups.iter_mut().find(|(s, _)| *s == scope) {
                    Some((_, v)) => v.push(p.clone()),
                    None => groups.push((scope, vec![p.clone()])),
                }
            }
            for (scope, objects) in groups {
                let model = session
                    .schema_model(request(scope.clone(), objects.clone(), true))
                    .await?;
                for wanted in &objects {
                    match model.tables.iter().find(|t| t.path == *wanted) {
                        Some(t) => tables.push(t.clone()),
                        None => {
                            return Err(Error::NotFound(
                                wanted
                                    .last()
                                    .map(|s| s.name.clone())
                                    .unwrap_or_else(|| "table".into()),
                            ));
                        }
                    }
                }
                ddl_scopes.push((scope, objects));
            }
        }
        ExportSource::Query { .. } => {}
        ExportSource::Keys { .. } => return Err(Error::Unsupported("export_keys".into())),
    }
    let is_query = matches!(spec.source, ExportSource::Query { .. });
    let database = match &spec.source {
        ExportSource::Query { database, .. } => database.clone(),
        _ => {
            let mut dbs: Vec<String> = tables
                .iter()
                .filter_map(|t| crate::conn::database_of(&t.path))
                .collect();
            dbs.dedup();
            dbs.sort();
            dbs.dedup();
            if dbs.len() > 1 {
                return Err(Error::Other(format!(
                    "the tables belong to several databases ({}); export them one database at a time",
                    dbs.join(", ")
                )));
            }
            dbs.pop()
        }
    };
    if !is_query && tables.is_empty() {
        return Err(Error::Other("nothing to export: no tables found".into()));
    }
    tables.sort_by(|a, b| {
        let qa = qualifier(Some(&a.path), a.schema.as_deref(), ctx.dialect);
        let qb = qualifier(Some(&b.path), b.schema.as_deref(), ctx.dialect);
        (qa, &a.name).cmp(&(qb, &b.name))
    });
    let mut qualifiers: Vec<Option<String>> = tables
        .iter()
        .map(|t| qualifier(Some(&t.path), t.schema.as_deref(), ctx.dialect))
        .collect();
    qualifiers.dedup();
    let multi_schema = qualifiers.len() > 1;

    // A query is always exported as data.
    let content = if is_query {
        ExportContent::Data
    } else {
        spec.content
    };
    let multi_file_format = matches!(spec.format, ExportFormat::Csv(_) | ExportFormat::Json(_));
    let files = if multi_file_format {
        let data = if content == ExportContent::Schema {
            0
        } else if is_query {
            1
        } else {
            tables.len()
        };
        data + usize::from(content != ExportContent::Data)
    } else {
        1
    };
    Ok(Work {
        tables,
        ddl_scopes,
        multi_schema,
        database,
        files,
        content,
    })
}

/// Destination path of the output: a `.zip` when several files are produced.
fn output_path(spec: &ExportSpec, work: &Work, warnings: &mut Vec<String>) -> PathBuf {
    let dest = PathBuf::from(&spec.destination.path);
    let zip_needed = work.files > 1;
    let is_zip = dest
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    if zip_needed && !is_zip {
        let renamed = dest.with_extension("zip");
        warnings.push(format!(
            "several files are produced, so the output is {}",
            renamed
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        renamed
    } else {
        dest
    }
}

fn check_destination(spec: &ExportSpec, work: &Work) -> Result<()> {
    let mut ignored = Vec::new();
    let path = output_path(spec, work, &mut ignored);
    // The user picked `dest`; a `.zip` next to it that they did not pick is never overwritten.
    if path != Path::new(&spec.destination.path) && path.exists() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("file already exists: {}", path.display()),
        )));
    }
    crate::fsio::check_destination(&path, spec.destination.overwrite)
}

// ---------------------------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------------------------

struct Progress<'a> {
    reporter: &'a dyn JobReporter,
    objects_total: u32,
    state: JobProgress,
}

impl<'a> Progress<'a> {
    fn new(reporter: &'a dyn JobReporter, work: &Work) -> Self {
        let estimates: Vec<Option<u64>> = work.tables.iter().map(|t| t.estimated_rows).collect();
        let rows_total = if !estimates.is_empty() && estimates.iter().all(Option::is_some) {
            Some(estimates.iter().flatten().sum())
        } else {
            None
        };
        let objects_total = work.tables.len().max(1) as u32;
        Progress {
            reporter,
            objects_total,
            state: JobProgress {
                phase: "schema".into(),
                objects_total: Some(objects_total),
                rows_total,
                ..JobProgress::default()
            },
        }
    }

    fn phase(&mut self, phase: &str, object: Option<String>) {
        self.state.phase = phase.into();
        self.state.object = object;
        self.reporter.progress(&self.state);
    }

    fn rows(&mut self, n: u64, bytes: u64) {
        self.state.rows_done += n;
        self.state.bytes_done = bytes;
        self.reporter.progress(&self.state);
    }

    fn object_done(&mut self) {
        self.state.objects_done = (self.state.objects_done + 1).min(self.objects_total);
        self.reporter.progress(&self.state);
    }
}

// ---------------------------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------------------------

async fn run(
    ctx: &ExportContext,
    spec: &ExportSpec,
    tuning: &ExportTuning,
    work: &Work,
    conn: &Arc<Conn>,
    cancel: &CancelToken,
    warnings: &mut Vec<String>,
) -> Result<ExportReport> {
    let dest = output_path(spec, work, warnings);
    let mut writer = create_writer(WriterConfig {
        format: spec.format.clone(),
        dialect: ctx.dialect,
        binary: spec.values.binary,
        content: work.content,
        dest,
        overwrite: spec.destination.overwrite,
        zip: work.files > 1,
        tuning: *tuning,
    })?;
    let mut progress = Progress::new(&*ctx.reporter, work);
    let is_sql = matches!(spec.format, ExportFormat::Sql(_));
    let mut rows_total = 0u64;
    let mut objects = 0u32;

    // Structure first (SQL: the script's DDL; other formats collect it from the table models).
    let mut post: Vec<DdlItem> = Vec::new();
    if is_sql && work.content != ExportContent::Data {
        progress.phase("schema", None);
        let ExportFormat::Sql(sql_opts) = &spec.format else {
            unreachable!()
        };
        let (pre, after) = generate_ddl(ctx, work, &sql_opts.ddl).await?;
        writer.write_ddl(DdlSection::PreData, &pre)?;
        post = after;
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }

    match &spec.source {
        ExportSource::Query { text, database } => {
            check_query_is_read_only(ctx.dialect, text)?;
            progress.phase("data", Some("query".into()));
            let meta = ObjectMeta {
                path: None,
                name: "query".into(),
                columns: Vec::new(),
                details: None,
                estimated_rows: None,
            };
            let mut req = QueryRequest::new(text.clone());
            req.database = database.clone();
            let n = stream_object(
                conn,
                cancel,
                &mut *writer,
                meta,
                Source::Query(req),
                &mut progress,
                warnings,
            )
            .await?;
            rows_total += n;
            objects += 1;
        }
        _ => {
            for table in &work.tables {
                if cancel.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let name = entry_name(table, work.multi_schema, ctx.dialect);
                progress.phase("data", Some(name.clone()));
                let columns: Vec<ColumnMeta> = table.columns.iter().map(column_meta).collect();
                let meta = ObjectMeta {
                    path: Some(table.path.clone()),
                    name,
                    columns,
                    details: Some(table.clone()),
                    estimated_rows: table.estimated_rows,
                };
                if work.content == ExportContent::Schema {
                    if !is_sql {
                        writer.begin_object(&meta)?;
                        writer.end_object()?;
                    }
                } else {
                    let n = stream_object(
                        conn,
                        cancel,
                        &mut *writer,
                        meta,
                        Source::Table(table, ctx.dialect),
                        &mut progress,
                        warnings,
                    )
                    .await?;
                    rows_total += n;
                }
                objects += 1;
                progress.object_done();
            }
        }
    }
    if is_sql && work.content != ExportContent::Data {
        writer.write_ddl(DdlSection::PostData, &post)?;
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    progress.phase("finish", None);
    warnings.extend(writer.warnings());
    let files = writer.finish()?;
    let bytes: u64 = files
        .iter()
        .filter_map(|f| std::fs::metadata(f).ok())
        .map(|m| m.len())
        .sum();
    Ok(ExportReport {
        files: files
            .iter()
            .map(|f| f.to_string_lossy().into_owned())
            .collect(),
        objects,
        rows: rows_total,
        bytes,
        warnings: std::mem::take(warnings),
    })
}

fn column_meta(c: &ColumnDetail) -> ColumnMeta {
    ColumnMeta {
        name: c.name.clone(),
        type_name: c.type_name.clone(),
        value_kind: c.value_kind,
        nullable: Some(c.nullable),
    }
}

/// Entry / sheet / table name: `schema.table` when tables of several schemas are exported.
fn entry_name(t: &TableModel, multi_schema: bool, dialect: ScriptDialect) -> String {
    if multi_schema {
        if let Some(q) = qualifier(Some(&t.path), t.schema.as_deref(), dialect) {
            return format!("{q}.{}", t.name);
        }
    }
    t.name.clone()
}

/// An export may only read: refuse a query that writes (the lease transaction is read-only too).
fn check_query_is_read_only(dialect: ScriptDialect, text: &str) -> Result<()> {
    // Exactly one statement: nothing else may ride along with the exported query.
    let statements = dbstudio_sql::split(dialect, text)
        .iter()
        .filter(|r| {
            text.chars()
                .skip(r.start as usize)
                .take((r.end - r.start) as usize)
                .any(|c| !c.is_whitespace())
        })
        .count();
    if statements != 1 {
        return Err(Error::Other(
            "the query to export must be exactly one statement".into(),
        ));
    }
    let analysis = dbstudio_sql::analyze(dialect, text);
    let policy = dbstudio_sql::Policy {
        read_only: true,
        production: false,
        confirm_text: String::new(),
    };
    match dbstudio_sql::check(&policy, &analysis) {
        dbstudio_sql::Verdict::Deny(e) => Err(e),
        _ => Ok(()),
    }
}

/// `(PreData, PostData)` items of every scope; everything of the first part precedes the data.
async fn generate_ddl(
    ctx: &ExportContext,
    work: &Work,
    options: &DdlOptions,
) -> Result<(Vec<DdlItem>, Vec<DdlItem>)> {
    let mut pre = Vec::new();
    let mut post = Vec::new();
    for (scope, objects) in &work.ddl_scopes {
        let items = ctx
            .session
            .generate_ddl(DdlRequest {
                scope: scope.clone(),
                objects: objects.clone(),
                options: options.clone(),
            })
            .await?;
        for item in items {
            match item.section {
                DdlSection::PreData => pre.push(item),
                DdlSection::PostData => post.push(item),
            }
        }
    }
    Ok((pre, post))
}

enum Source<'a> {
    Table(&'a TableModel, ScriptDialect),
    Query(QueryRequest),
}

/// How the result sets of one object are treated.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SetMode {
    /// First result set only (a query).
    First,
    /// Every statement continues the same table (paged reads).
    Continue,
}

/// Forwards rows to the writer, counts them and watches for cancellation.
struct ObjectSink<'a, 'b> {
    writer: &'a mut dyn TableWriter,
    meta: ObjectMeta,
    mode: SetMode,
    started: bool,
    skipping: bool,
    ignored_sets: u32,
    rows: u64,
    page_rows: u64,
    cancel: &'a CancelToken,
    progress: &'a mut Progress<'b>,
}

impl ResultSink for ObjectSink<'_, '_> {
    fn on_columns(&mut self, columns: Vec<ColumnMeta>) -> Result<()> {
        if self.started {
            if self.mode == SetMode::First {
                self.skipping = true;
                self.ignored_sets += 1;
            }
            self.page_rows = 0;
            return Ok(());
        }
        self.meta.columns = columns;
        self.writer.begin_object(&self.meta)?;
        self.started = true;
        Ok(())
    }

    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        if self.skipping {
            return Ok(());
        }
        if self.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let n = rows.len() as u64;
        self.writer.on_rows(rows)?;
        self.rows += n;
        self.page_rows += n;
        self.progress.rows(n, self.writer.bytes());
        Ok(())
    }

    fn on_message(&mut self, _message: String) -> Result<()> {
        Ok(())
    }

    fn on_statement_end(&mut self, end: StatementEnd) -> Result<()> {
        if let EndOutcome::Rows { row_count, .. } = end.outcome {
            if self.mode == SetMode::Continue {
                self.page_rows = row_count;
            }
        }
        Ok(())
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

/// `SELECT col, ... FROM schema.table` in the dialect of the source.
pub(crate) fn select_all(table: &TableModel, dialect: ScriptDialect) -> String {
    let q = Dialect::new(dialect);
    let name = match qualifier(Some(&table.path), table.schema.as_deref(), dialect) {
        Some(s) => format!("{}.{}", q.quote_ident(&s), q.quote_ident(&table.name)),
        None => q.quote_ident(&table.name),
    };
    let columns = if table.columns.is_empty() {
        "*".to_string()
    } else {
        table
            .columns
            .iter()
            .map(|c| q.quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!("SELECT {columns} FROM {name}")
}

/// Stream one table or query into the writer; returns the rows written.
async fn stream_object(
    conn: &Arc<Conn>,
    cancel: &CancelToken,
    writer: &mut dyn TableWriter,
    meta: ObjectMeta,
    source: Source<'_>,
    progress: &mut Progress<'_>,
    warnings: &mut Vec<String>,
) -> Result<u64> {
    let fallback_columns = meta.columns.clone();
    let mode = match &source {
        Source::Query(_) => SetMode::First,
        Source::Table(..) => SetMode::Continue,
    };
    let mut sink = ObjectSink {
        writer,
        meta,
        mode,
        started: false,
        skipping: false,
        ignored_sets: 0,
        rows: 0,
        page_rows: 0,
        cancel,
        progress,
    };
    match source {
        Source::Query(req) => {
            conn.execute_cancellable(cancel, req, &mut sink).await?;
        }
        Source::Table(table, dialect) => {
            stream_table(conn, cancel, table, dialect, &mut sink).await?;
        }
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if !sink.started {
        // Empty result without column information: fall back to the model's columns.
        sink.meta.columns = fallback_columns;
        sink.writer.begin_object(&sink.meta)?;
        sink.started = true;
    }
    sink.writer.end_object()?;
    if sink.ignored_sets > 0 {
        warnings.push(format!(
            "the query returned {} more result set(s); only the first one was exported",
            sink.ignored_sets
        ));
    }
    Ok(sink.rows)
}

async fn stream_table(
    conn: &Arc<Conn>,
    cancel: &CancelToken,
    table: &TableModel,
    dialect: ScriptDialect,
    sink: &mut ObjectSink<'_, '_>,
) -> Result<()> {
    if is_sql_dialect(dialect) {
        // Preferred: the driver's own table stream on the lease; otherwise a plain SELECT.
        if let Some(lease) = conn.lease() {
            let req = TableStreamRequest {
                query_id: Uuid::new_v4(),
                table: table.path.clone(),
                columns: Vec::new(),
                filters: Vec::new(),
                raw_where: None,
                order_by_key: false,
                batch_size: None,
            };
            match lease.stream_table(req, sink).await {
                Ok(_) => return Ok(()),
                Err(Error::Unsupported(_)) if !sink.started => {}
                Err(e) => return Err(map_cancel(e, cancel)),
            }
        }
        let req = QueryRequest::new(select_all(table, dialect));
        conn.execute_cancellable(cancel, req, sink).await?;
        return Ok(());
    }
    // Document / key-value sources: pages through `fetch_table`.
    let mut offset = 0u64;
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        sink.page_rows = 0;
        let req = TableDataRequest {
            query_id: Uuid::new_v4(),
            table: table.path.clone(),
            filters: Vec::new(),
            raw_where: None,
            sort: Vec::new(),
            offset,
            limit: PAGE_ROWS,
            batch_size: None,
        };
        let r = conn.session().fetch_table(req, sink).await;
        r.map_err(|e| map_cancel(e, cancel))?;
        if sink.page_rows < PAGE_ROWS {
            return Ok(());
        }
        offset += PAGE_ROWS;
    }
}

fn map_cancel(e: Error, cancel: &CancelToken) -> Error {
    if cancel.is_cancelled() {
        Error::Cancelled
    } else {
        e
    }
}
