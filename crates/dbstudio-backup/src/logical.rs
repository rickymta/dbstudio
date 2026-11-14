//! Integrated ("logical") backup: one SQL script per database written by the `dbstudio-export`
//! engine (structure from `generate_ddl` in PreData / PostData order, rows streamed through a
//! read-only repeatable-read lease), and the matching restore on a dedicated connection.
//!
//! Content of the tar (see `container`): `data/dump.sql` only (plus the preamble and manifest).
//! The script restores in one pass: PreData (schemas, types, sequences, tables, views,
//! routines), rows, then PostData (indexes, foreign keys, triggers, sequence values, comments).

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

use dbstudio_core::{
    CancelToken, ColumnMeta, DdlOptions, EndOutcome, Error, JobProgress, JobReporter, Lease,
    LeaseOptions, ObjectKind, ObjectPath, QueryRequest, Result, ResultSink, ScriptDialect, Session,
    StatementEnd, TxOptions, Value,
};
use dbstudio_export::{
    ExportContent, ExportContext, ExportDestination, ExportFormat, ExportSource, ExportSpec,
    RowError, SqlOptions, ValueFormat, run_export,
};
use dbstudio_sql::StreamSplitter;
use serde_json::{Value as Json, json};

use crate::partial::{self, Marker, Selection};
use uuid::Uuid;

use crate::container::{Scanned, SpoolEntry};

/// Name of the script inside the container.
pub const DUMP_ENTRY: &str = "data/dump.sql";

/// Things a logical backup does not contain (shown in the preamble and the restore preview).
pub const UNSUPPORTED_SCOPE: [&str; 5] = [
    "roles and grants",
    "server level settings",
    "MySQL events",
    "PostgreSQL large objects",
    "users and passwords",
];

/// Errors kept in the restore outcome when the script is allowed to continue.
const MAX_REPORTED_ERRORS: usize = 1000;

/// Refuse backups written by an earlier layout (several `.sql` files, container preamble
/// version 1): only the current single-script layout can be restored.
pub(crate) fn check_supported(pre: &Json) -> Result<()> {
    if pre["engine"] == "logical" && pre["formatVersion"].as_u64().unwrap_or(1) < 2 {
        return Err(Error::Unsupported(
            "this backup was written by an older DBStudio (integrated format 1) and cannot be \
             restored or verified by this version; restore it with the version that made it"
                .into(),
        ));
    }
    Ok(())
}

/// Database name of a target path, when it has one.
pub(crate) fn database_of(path: &ObjectPath) -> Option<String> {
    path.segments
        .iter()
        .find(|s| s.kind == ObjectKind::Database)
        .map(|s| s.name.clone())
}

fn dialect_name(d: ScriptDialect) -> Result<&'static str> {
    match d {
        ScriptDialect::Postgres => Ok("postgres"),
        ScriptDialect::MySql { .. } => Ok("mysql"),
        ScriptDialect::MsSql => Ok("mssql"),
        ScriptDialect::Oracle => Ok("oracle"),
        _ => Err(Error::Unsupported(
            "integrated backup is only available for SQL databases; use a native tool".into(),
        )),
    }
}

/// What the dump produced for one database.
pub(crate) struct Dump {
    pub entries: Vec<SpoolEntry>,
    pub preamble: Json,
    pub objects: u32,
    pub rows: u64,
}

/// Source of the export for `targets` (all in `database`).
fn export_source(database: &str, targets: &[ObjectPath]) -> Result<ExportSource> {
    let db_path = ObjectPath::root().child(ObjectKind::Database, database);
    let kinds: Vec<Option<ObjectKind>> = targets
        .iter()
        .map(|t| {
            t.segments
                .iter()
                .rfind(|s| s.kind != ObjectKind::Folder)
                .map(|s| s.kind)
        })
        .collect();
    if targets.is_empty() || kinds.contains(&Some(ObjectKind::Database)) {
        return Ok(ExportSource::Database { path: db_path });
    }
    if kinds
        .iter()
        .all(|k| matches!(k, Some(ObjectKind::Table | ObjectKind::View)))
    {
        return Ok(ExportSource::Tables {
            paths: targets.to_vec(),
        });
    }
    if targets.len() == 1 && kinds[0] == Some(ObjectKind::Schema) {
        return Ok(ExportSource::Schema {
            path: targets[0].clone(),
        });
    }
    Err(Error::Other(
        "choose a whole database, one schema, or a list of tables to back up".into(),
    ))
}

/// Dump the objects of `database` selected by `targets` (empty = the whole database).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dump(
    session: &Arc<dyn Session>,
    kind: dbstudio_core::DbKind,
    spool: &Path,
    database: &str,
    targets: &[ObjectPath],
    content: ExportContent,
    ddl: &DdlOptions,
    source: Json,
    reporter: &Arc<dyn JobReporter>,
) -> Result<Dump> {
    let sd = session.script_dialect();
    match sd {
        ScriptDialect::MongoShell => {
            return crate::nosql::dump_mongo(
                session, spool, database, targets, content, source, reporter,
            )
            .await;
        }
        ScriptDialect::RedisCli => {
            return crate::nosql::dump_redis(session, spool, database, content, source, reporter)
                .await;
        }
        _ => {}
    }
    let dname = dialect_name(sd)?;
    let src = export_source(database, targets)?;
    let mut ddl = ddl.clone();
    if matches!(sd, ScriptDialect::MySql { .. }) {
        // MySQL names carry the database; a restore into another name must not.
        ddl.qualify_schema = false;
    }
    let file = spool.join("dump.sql");
    let spec = ExportSpec {
        connection_id: Uuid::nil(),
        source: src,
        content,
        format: ExportFormat::Sql(SqlOptions {
            ddl,
            wrap_in_transaction: false,
            disable_fk_checks: matches!(sd, ScriptDialect::MySql { .. }),
            ..SqlOptions::default()
        }),
        destination: ExportDestination {
            path: file.to_string_lossy().into_owned(),
            overwrite: true,
        },
        values: ValueFormat::default(),
    };
    let ctx = ExportContext {
        session: Arc::clone(session),
        kind,
        dialect: sd,
        reporter: Arc::clone(reporter),
    };
    let report = run_export(ctx, spec).await?;
    for w in &report.warnings {
        reporter.log(w);
    }
    let mut entry = crate::ops::spool_entry_from_file(DUMP_ENTRY, &file)?;
    entry.rows = report.rows;
    let preamble = json!({
        "formatVersion": 3,
        "engine": "logical",
        "createdAt": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "database": database,
        "source": source,
        "dialect": dname,
        "content": match content {
            ExportContent::Schema => "schema",
            ExportContent::Data => "data",
            ExportContent::Both => "both",
        },
        "dumpFile": DUMP_ENTRY,
        "objects": report.object_list,
        "markerNonce": report.marker_nonce,
        "tables": report.objects,
        "warnings": report.warnings,
        "unsupported": UNSUPPORTED_SCOPE,
    });
    Ok(Dump {
        objects: report.objects,
        rows: report.rows,
        entries: vec![entry],
        preamble,
    })
}

// ---------------------------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------------------------

/// Discards result sets, remembers affected rows.
pub(crate) struct NullSink;

impl ResultSink for NullSink {
    fn on_columns(&mut self, _: Vec<ColumnMeta>) -> Result<()> {
        Ok(())
    }
    fn on_rows(&mut self, _: Vec<Vec<Value>>) -> Result<()> {
        Ok(())
    }
    fn on_message(&mut self, _: String) -> Result<()> {
        Ok(())
    }
    fn on_statement_end(&mut self, end: StatementEnd) -> Result<()> {
        let _ = matches!(end.outcome, EndOutcome::Done);
        Ok(())
    }
}

/// One connection the script runs on: a lease (session state such as `search_path` and
/// `FOREIGN_KEY_CHECKS` and an optional transaction) or, without lease support, the session.
struct Target {
    session: Arc<dyn Session>,
    lease: Option<Box<dyn Lease>>,
    database: String,
}

impl Target {
    async fn open(session: &Arc<dyn Session>, database: &str) -> Result<Target> {
        let opts = LeaseOptions {
            database: Some(database.to_string()),
            ..LeaseOptions::default()
        };
        let lease = match session.open_lease(opts).await {
            Ok(l) => Some(l),
            Err(Error::Unsupported(_)) => None,
            Err(e) => return Err(e),
        };
        Ok(Target {
            session: Arc::clone(session),
            lease,
            database: database.to_string(),
        })
    }

    async fn run(&self, token: &CancelToken, sql: String) -> Result<()> {
        let mut req = QueryRequest::new(sql);
        let query_id = req.query_id;
        let mut sink = NullSink;
        let exec = async {
            match &self.lease {
                Some(l) => l.execute(req, &mut sink).await.map(|_| ()),
                None => {
                    req.database = Some(self.database.clone());
                    self.session.execute(req, &mut sink).await.map(|_| ())
                }
            }
        };
        tokio::select! {
            r = exec => r,
            () = token.cancelled() => {
                // Ask the server to stop the statement, then report the cancellation.
                match &self.lease {
                    Some(l) => { let _ = l.cancel(query_id).await; }
                    None => { let _ = self.session.cancel(query_id).await; }
                }
                Err(Error::Cancelled)
            }
        }
    }

    async fn close(&self) {
        if let Some(l) = &self.lease {
            let _ = l.close().await;
        }
    }
}

fn same_family(a: &str, b: ScriptDialect) -> bool {
    matches!(
        (a, b),
        ("postgres", ScriptDialect::Postgres)
            | ("mysql", ScriptDialect::MySql { .. })
            | ("mssql", ScriptDialect::MsSql)
            | ("oracle", ScriptDialect::Oracle)
    )
}

/// Statements that empty the target before the script runs (`clean`). `charset` is the
/// character set and collation of the MySQL database being replaced, kept for the new one.
///
/// PostgreSQL drops every schema the backup contains with `CASCADE`: **all** objects in those
/// schemas go, including ones that are not in the backup; it runs inside the restore
/// transaction, so a failed restore keeps the old content. MySQL drops and re-creates the
/// database: that commits at once and cannot be rolled back.
pub fn clean_statements(
    pre: &Json,
    sd: ScriptDialect,
    database: &str,
    charset: Option<(String, String)>,
) -> Vec<String> {
    let q = dbstudio_sql::Dialect::new(sd);
    match sd {
        ScriptDialect::Postgres => {
            // Version 3 objects carry `schema`; earlier files only `name = "schema.table"`.
            let mut schemas: Vec<String> = pre["objects"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| match o["schema"].as_str() {
                    Some(s) => Some(s.to_string()),
                    None => o["name"]
                        .as_str()
                        .and_then(|n| n.split_once('.').map(|(s, _)| s.to_string())),
                })
                .collect();
            schemas.sort();
            schemas.dedup();
            if schemas.is_empty() {
                schemas.push("public".into());
            }
            // The script creates every schema itself except `public`, which always exists.
            schemas
                .iter()
                .flat_map(|s| {
                    let mut v = vec![format!(
                        "DROP SCHEMA IF EXISTS {} CASCADE",
                        q.quote_ident(s)
                    )];
                    if s == "public" {
                        v.push(format!("CREATE SCHEMA {}", q.quote_ident(s)));
                    }
                    v
                })
                .collect()
        }
        ScriptDialect::MySql { .. } => {
            let create = match charset {
                Some((cs, coll)) if is_word(&cs) && is_word(&coll) => format!(
                    "CREATE DATABASE {} CHARACTER SET {cs} COLLATE {coll}",
                    q.quote_ident(database)
                ),
                _ => format!("CREATE DATABASE {}", q.quote_ident(database)),
            };
            vec![
                format!("DROP DATABASE IF EXISTS {}", q.quote_ident(database)),
                create,
            ]
        }
        _ => Vec::new(),
    }
}

fn is_word(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The statements a `clean` restore of a backup with `pre` would run on a `dialect` server
/// (`postgres` | `mysql`); shown to the user before they confirm.
pub fn clean_preview(pre: &Json, dialect: &str, database: &str) -> Vec<String> {
    let sd = match dialect {
        "postgres" => ScriptDialect::Postgres,
        "mysql" => ScriptDialect::mysql(false, true),
        _ => return Vec::new(),
    };
    clean_statements(pre, sd, database, None)
}

/// Character set and collation of a MySQL database, when it exists.
async fn mysql_charset(session: &Arc<dyn Session>, database: &str) -> Option<(String, String)> {
    let q = dbstudio_sql::Dialect::new(session.script_dialect());
    let sql = format!(
        "SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA \
         WHERE SCHEMA_NAME = {}",
        q.literal_text(database)
    );
    let r = dbstudio_core::execute_collect(session.as_ref(), QueryRequest::new(sql))
        .await
        .ok()?;
    match &r.first()?.outcome {
        dbstudio_core::StatementOutcome::Rows(rs) => match rs.rows.first()?.as_slice() {
            [Value::Text(a), Value::Text(b)] => Some((a.clone(), b.clone())),
            _ => None,
        },
        _ => None,
    }
}

/// Restore the extracted logical backup into `session`.
///
/// The script runs on its own lease. On PostgreSQL the whole restore is one transaction (all or
/// nothing) unless `continue_on_error` is set; MySQL commits DDL implicitly, so a failed
/// restore leaves what was created so far.
pub(crate) async fn restore(
    session: &Arc<dyn Session>,
    scanned: &Scanned,
    spec: &crate::RestoreSpec,
    reporter_arc: &Arc<dyn JobReporter>,
) -> Result<crate::RestoreOutcome> {
    let reporter: &dyn JobReporter = reporter_arc.as_ref();
    let pre = &scanned.preamble;
    match pre["engine"].as_str() {
        Some("logical-mongo") => {
            return crate::nosql::restore_mongo(session, scanned, spec, reporter_arc).await;
        }
        Some("logical-redis") => {
            return crate::nosql::restore_redis(session, scanned, spec, reporter_arc).await;
        }
        _ => {}
    }
    let src_dialect = pre["dialect"].as_str().unwrap_or("");
    let sd = session.script_dialect();
    if dialect_name(sd).is_err() || !same_family(src_dialect, sd) {
        return Err(Error::Unsupported(format!(
            "this backup was made from {src_dialect}; restore it into a server of the same kind"
        )));
    }
    let selection = if spec.objects.is_empty() {
        None
    } else if pre["formatVersion"].as_u64().unwrap_or(1) < 3 {
        return Err(Error::Unsupported(
            "this backup was written before selected objects could be restored; restore the \
             whole backup"
                .into(),
        ));
    } else {
        let objects: Vec<crate::BackupObject> =
            serde_json::from_value(pre["objects"].clone()).unwrap_or_default();
        let sel = Selection::resolve(&objects, &spec.objects)?;
        if !sel.support.is_empty() {
            reporter.log(&format!(
                "also restoring supporting objects: {}",
                sel.support.join(", ")
            ));
        }
        // Without the file's nonce a marker cannot be told from data: whole restore only.
        let nonce = pre["markerNonce"]
            .as_str()
            .filter(|n| !n.is_empty())
            .ok_or_else(|| {
                Error::Unsupported(
                    "this backup has no marker token; restore the whole backup".into(),
                )
            })?
            .to_string();
        Some((sel, nonce))
    };
    let target_db = spec
        .target_database
        .clone()
        .or_else(|| pre["database"].as_str().map(str::to_string))
        .ok_or_else(|| Error::Other("target database is not specified".into()))?;
    let dump_name = pre["dumpFile"].as_str().unwrap_or(DUMP_ENTRY);
    let dump_path = scanned
        .files
        .get(dump_name)
        .ok_or_else(|| Error::Integrity("the script is missing from the backup".into()))?;
    let cancel = reporter.cancel_token().clone();
    let mut progress = JobProgress {
        phase: "restore".into(),
        objects_total: pre["tables"].as_u64().map(|n| n as u32),
        ..Default::default()
    };
    reporter.progress(&progress);

    let q = dbstudio_sql::Dialect::new(sd);
    if spec.create_database {
        let (sql, over) = match sd {
            ScriptDialect::Postgres => (
                format!("CREATE DATABASE {}", q.quote_ident(&target_db)),
                Some("postgres".to_string()),
            ),
            ScriptDialect::MsSql => (
                format!(
                    "IF DB_ID(N'{}') IS NULL CREATE DATABASE {}",
                    target_db.replace('\'', "''"),
                    q.quote_ident(&target_db)
                ),
                Some("master".to_string()),
            ),
            _ => (
                format!(
                    "CREATE DATABASE IF NOT EXISTS {}",
                    q.quote_ident(&target_db)
                ),
                None,
            ),
        };
        let mut req = QueryRequest::new(sql);
        req.database = over;
        if let Err(e) = session.execute(req, &mut NullSink).await {
            // An existing database is fine: the script goes into it.
            if !e.message().contains("already exists") {
                return Err(e);
            }
        }
    }
    if spec.clean && matches!(sd, ScriptDialect::MsSql) {
        return Err(Error::Unsupported(
            "a clean restore is not available for SQL Server: restore into a new database".into(),
        ));
    }
    // MySQL `clean` replaces the whole database: do it on the session, before the lease opens.
    let clean = if spec.clean {
        if spec.target_database.as_deref().is_none_or(str::is_empty) {
            return Err(Error::Other(
                "a clean restore needs the target database to be named explicitly".into(),
            ));
        }
        let charset = if matches!(sd, ScriptDialect::MySql { .. }) {
            mysql_charset(session, &target_db).await
        } else {
            None
        };
        clean_statements(pre, sd, &target_db, charset)
    } else {
        Vec::new()
    };
    if matches!(sd, ScriptDialect::MySql { .. }) {
        for sql in &clean {
            let mut req = QueryRequest::new(sql.clone());
            req.database = None;
            session.execute(req, &mut NullSink).await?;
        }
    }

    let target = Target::open(session, &target_db).await?;
    let single_tx =
        matches!(sd, ScriptDialect::Postgres) && !spec.continue_on_error && target.lease.is_some();
    let result = run_script(
        &target,
        &cancel,
        sd,
        &clean,
        dump_path,
        spec,
        selection.as_ref().map(|(s, n)| (s, n.as_str())),
        single_tx,
        &mut progress,
        reporter,
    )
    .await;
    target.close().await;
    let (errors, rows_run) = result?;
    let rows = scanned
        .manifest
        .entries
        .iter()
        .find(|e| e.path == dump_name)
        .map_or(0, |e| e.rows);
    let (objects, rows) = match &selection {
        // Rows of a partial restore are the ones of the chosen objects.
        Some((sel, _)) => selected_counts(pre, sel),
        None => (pre["tables"].as_u64().unwrap_or(0) as u32, rows),
    };
    let _ = rows_run;
    Ok(crate::RestoreOutcome {
        objects,
        rows: if errors.is_empty() { rows } else { 0 },
        errors,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_script(
    target: &Target,
    cancel: &CancelToken,
    sd: ScriptDialect,
    clean: &[String],
    path: &Path,
    spec: &crate::RestoreSpec,
    selection: Option<(&Selection, &str)>,
    single_tx: bool,
    progress: &mut JobProgress,
    reporter: &dyn JobReporter,
) -> Result<(Vec<RowError>, u64)> {
    // Same session state as the export: values are written in UTC.
    let zone = match sd {
        ScriptDialect::MySql { .. } => Some("SET time_zone = '+00:00'"),
        ScriptDialect::Postgres => Some("SET TIME ZONE 'UTC'"),
        _ => None,
    };
    if let Some(sql) = zone {
        target.run(cancel, sql.into()).await?;
    }
    if single_tx {
        if let Some(l) = &target.lease {
            l.begin(TxOptions {
                read_only: false,
                isolation: None,
            })
            .await?;
        }
    }
    // PostgreSQL `clean` runs inside the transaction: a failed restore keeps the old content.
    if matches!(sd, ScriptDialect::Postgres) {
        for sql in clean {
            target.run(cancel, sql.clone()).await?;
        }
    }
    let file =
        File::open(path).map_err(|e| Error::Other(format!("cannot read the script: {e}")))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    progress.bytes_total = Some(size);
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let mut splitter = StreamSplitter::new(script_dialect_of(sd));
    let mut errors: Vec<RowError> = Vec::new();
    let (mut n, mut bytes) = (0u64, 0u64);
    // SQL Server: rows go in with their identity values.
    let mut identity = matches!(sd, ScriptDialect::MsSql).then(IdentityInsert::default);
    let mut line = String::new();
    let mut eof = false;
    // Partial restore: the block the next statements belong to.
    let mut gate = Gate::default();
    while !eof {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| Error::Other(format!("cannot read the script: {e}")))?;
        bytes += read as u64;
        if let Some((sel, nonce)) = selection {
            if read > 0 && partial::looks_like_marker(&line) {
                // A marker only counts between statements: the same text inside a string
                // literal is data.
                let boundary = splitter
                    .clone()
                    .finish()
                    .iter()
                    .all(|s| partial::only_comments(s));
                if boundary {
                    if let Some(m) = partial::parse_marker(&line, nonce) {
                        gate = Gate::of(&m, sel);
                        continue;
                    }
                }
            }
        }
        let statements = if read == 0 {
            eof = true;
            splitter.clone().finish()
        } else {
            splitter.push(&line)
        };
        for stmt in statements {
            if stmt.trim().is_empty() {
                continue;
            }
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if let Some((sel, _)) = selection {
                if !gate.run {
                    continue;
                }
                if gate.post {
                    if let Some((schema, table)) = partial::referenced_table(&stmt) {
                        if !sel.has_table(schema.as_deref(), &table) {
                            reporter.log(&format!(
                                "skipped a constraint that references {}, which is not restored",
                                match &schema {
                                    Some(s) => format!("{s}.{table}"),
                                    None => table.clone(),
                                }
                            ));
                            continue;
                        }
                    }
                }
            }
            n += 1;
            match run_statement(target, cancel, identity.as_mut(), stmt).await {
                Ok(()) => {}
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) if spec.continue_on_error => {
                    if errors.len() < MAX_REPORTED_ERRORS {
                        errors.push(RowError {
                            line: n,
                            message: e.message(),
                        });
                    }
                }
                Err(e) => {
                    return Err(Error::Query {
                        message: format!("statement {n}: {}", e.message()),
                        position: None,
                    });
                }
            }
            if n % 50 == 0 {
                progress.rows_done = n;
                progress.bytes_done = bytes;
                reporter.progress(progress);
            }
        }
    }
    if single_tx {
        if let Some(l) = &target.lease {
            l.commit().await?;
        }
    }
    progress.rows_done = n;
    progress.bytes_done = bytes;
    reporter.progress(progress);
    Ok((errors, n))
}

/// `SET IDENTITY_INSERT` handling for SQL Server: the export script inserts explicit values
/// into identity columns, which the server only accepts with this switch on. The switch lives
/// as long as the batch that sets it, so it goes in the same batch as the `INSERT`. A table
/// without an identity column answers it with an error: the table is then remembered and its
/// inserts run as they are.
#[derive(Default)]
struct IdentityInsert {
    /// Quoted names of the tables known to have no identity column.
    plain: std::collections::HashSet<String>,
}

fn quote_table(t: &(Option<String>, String)) -> String {
    let q = |s: &str| format!("[{}]", s.replace(']', "]]"));
    match &t.0 {
        Some(schema) => format!("{}.{}", q(schema), q(&t.1)),
        None => q(&t.1),
    }
}

async fn run_statement(
    target: &Target,
    cancel: &CancelToken,
    identity: Option<&mut IdentityInsert>,
    stmt: String,
) -> Result<()> {
    let Some(id) = identity else {
        return target.run(cancel, stmt).await;
    };
    if partial::insert_target(&stmt).is_none() {
        return target.run(cancel, stmt).await;
    }
    // T-SQL batches end at `GO`, so one batch can hold the inserts of several tables: each
    // `INSERT` gets its own switch. The split is lexical (quote aware), not textual.
    let mut splitter = StreamSplitter::new(ScriptDialect::Postgres);
    let mut pieces = splitter.push(&stmt);
    pieces.extend(splitter.finish());
    for piece in pieces {
        if piece.trim().is_empty() {
            continue;
        }
        let Some(table) = partial::insert_target(&piece) else {
            target.run(cancel, piece).await?;
            continue;
        };
        let name = quote_table(&table);
        if id.plain.contains(&name) {
            target.run(cancel, piece).await?;
            continue;
        }
        let wrapped =
            format!("SET IDENTITY_INSERT {name} ON;\n{piece}\n;SET IDENTITY_INSERT {name} OFF;");
        match target.run(cancel, wrapped).await {
            Err(e) if e.message().contains("does not have the identity property") => {
                id.plain.insert(name);
                target.run(cancel, piece).await?;
            }
            Err(e) => return Err(e),
            Ok(()) => {}
        }
    }
    Ok(())
}

/// Which statements of the script run during a partial restore.
struct Gate {
    run: bool,
    post: bool,
}

impl Default for Gate {
    fn default() -> Self {
        // Statements before the first marker (the script prologue) always run.
        Gate {
            run: true,
            post: false,
        }
    }
}

impl Gate {
    fn of(marker: &Marker, sel: &Selection) -> Gate {
        match marker {
            Marker::Common => Gate::default(),
            Marker::Object {
                kind,
                schema,
                name,
                phase,
            } => Gate {
                run: sel.includes(*kind, schema, name),
                post: *phase == crate::BackupPhase::Post,
            },
        }
    }
}

/// Objects and rows of the chosen tables (from the preamble).
fn selected_counts(pre: &Json, sel: &Selection) -> (u32, u64) {
    let objects: Vec<crate::BackupObject> =
        serde_json::from_value(pre["objects"].clone()).unwrap_or_default();
    let mut n = 0;
    let mut rows = 0;
    for o in objects.iter().filter(|o| {
        matches!(
            o.kind,
            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
        ) && sel.includes(o.kind, &o.schema, &o.name)
    }) {
        n += 1;
        rows += o.rows.unwrap_or(0);
    }
    (n, rows)
}

fn script_dialect_of(sd: ScriptDialect) -> ScriptDialect {
    match sd {
        ScriptDialect::MySql { mariadb, .. } => ScriptDialect::mysql(mariadb, true),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(n: &str) -> ObjectPath {
        ObjectPath::root().child(ObjectKind::Database, n)
    }

    #[test]
    fn export_source_follows_the_targets() {
        assert!(matches!(
            export_source("app", &[]).unwrap(),
            ExportSource::Database { .. }
        ));
        assert!(matches!(
            export_source("app", &[db("app")]).unwrap(),
            ExportSource::Database { .. }
        ));
        let t = db("app").child(ObjectKind::Table, "t");
        assert!(matches!(
            export_source("app", std::slice::from_ref(&t)).unwrap(),
            ExportSource::Tables { .. }
        ));
        let s = db("app").child(ObjectKind::Schema, "s");
        assert!(matches!(
            export_source("app", std::slice::from_ref(&s)).unwrap(),
            ExportSource::Schema { .. }
        ));
        assert!(export_source("app", &[s.clone(), s]).is_err());
        drop(t);
    }

    #[test]
    fn clean_replaces_schemas_or_the_database() {
        let pre =
            json!({"objects": [{"name": "app.users"}, {"name": "app.orders"}, {"name": "x.t"}]});
        let pg = clean_statements(&pre, ScriptDialect::Postgres, "d", None);
        assert_eq!(pg[0], "DROP SCHEMA IF EXISTS \"app\" CASCADE");
        assert_eq!(pg.len(), 2);
        let my = clean_statements(&pre, ScriptDialect::mysql(false, true), "d", None);
        assert_eq!(my, ["DROP DATABASE IF EXISTS `d`", "CREATE DATABASE `d`"]);
        let kept = clean_statements(
            &pre,
            ScriptDialect::mysql(false, true),
            "d",
            Some(("utf8mb4".into(), "utf8mb4_0900_ai_ci".into())),
        );
        assert_eq!(
            kept[1],
            "CREATE DATABASE `d` CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci"
        );
        // Anything that is not a plain name is not spliced into SQL.
        let odd = clean_statements(
            &pre,
            ScriptDialect::mysql(false, true),
            "d",
            Some(("x; DROP".into(), "y".into())),
        );
        assert_eq!(odd[1], "CREATE DATABASE `d`");
        assert_eq!(clean_preview(&pre, "postgres", "d").len(), 2);
        assert_eq!(
            clean_statements(&json!({}), ScriptDialect::Postgres, "d", None),
            [
                "DROP SCHEMA IF EXISTS \"public\" CASCADE",
                "CREATE SCHEMA \"public\""
            ]
        );
    }

    #[test]
    fn a_marker_inside_a_literal_is_not_at_a_boundary() {
        // Row data with a real line break followed by a forged marker line.
        let mut sp = StreamSplitter::new(ScriptDialect::Postgres);
        let first = sp.push("INSERT INTO t VALUES ('x\n");
        assert!(first.is_empty());
        let forged = "-- dbstudio:object kind=table name=\"t\" phase=data nonce=abc\n";
        assert!(partial::looks_like_marker(forged));
        let at_boundary = sp
            .clone()
            .finish()
            .iter()
            .all(|s| partial::only_comments(s));
        assert!(!at_boundary, "inside a string literal");
        // Between statements it is one.
        let mut sp = StreamSplitter::new(ScriptDialect::Postgres);
        assert_eq!(sp.push("SELECT 1;\n").len(), 1);
        assert!(sp.finish().iter().all(|s| partial::only_comments(s)));
        // And without the file's nonce it is plain text either way.
        assert!(partial::parse_marker(forged, "0123456789abcdef").is_none());
    }

    #[test]
    fn dialect_families() {
        assert!(same_family("postgres", ScriptDialect::Postgres));
        assert!(!same_family("postgres", ScriptDialect::MsSql));
        assert_eq!(dialect_name(ScriptDialect::Oracle).unwrap(), "oracle");
    }
}
