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
    LeaseOptions, ModelDetail, ObjectKind, ObjectPath, QueryRequest, Result, ResultSink,
    SchemaModelRequest, ScriptDialect, Session, StatementEnd, TxOptions, Value,
};
use dbstudio_export::{
    ExportContent, ExportContext, ExportDestination, ExportFormat, ExportSource, ExportSpec,
    RowError, SqlOptions, ValueFormat, run_export,
};
use dbstudio_sql::StreamSplitter;
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::container::{Scanned, SpoolEntry};

/// Name of the script inside the container.
pub const DUMP_ENTRY: &str = "data/dump.sql";

/// Things a logical backup does not contain (shown in the preamble and the restore preview).
pub const UNSUPPORTED_SCOPE: [&str; 2] = ["roles and grants", "server level settings"];

/// Errors kept in the restore outcome when the script is allowed to continue.
const MAX_REPORTED_ERRORS: usize = 1000;

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

/// Names of the tables and views of `source` for the preamble (best effort).
async fn list_objects(session: &dyn Session, source: &ExportSource) -> Vec<Json> {
    let (scope, tables) = match source {
        ExportSource::Database { path } | ExportSource::Schema { path } => {
            (path.clone(), Vec::new())
        }
        ExportSource::Tables { paths } => (
            paths.first().cloned().map(scope_of).unwrap_or_default(),
            paths.clone(),
        ),
        _ => return Vec::new(),
    };
    let req = SchemaModelRequest {
        scope,
        tables,
        include_views: true,
        include_comments: false,
        detail: ModelDetail::Keys,
        sample_size: None,
    };
    match session.schema_model(req).await {
        Ok(m) => m
            .tables
            .iter()
            .map(|t| {
                let name = match &t.schema {
                    Some(s) => format!("{s}.{}", t.name),
                    None => t.name.clone(),
                };
                json!({"name": name, "kind": if t.kind == ObjectKind::View { "view" } else { "table" }})
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn scope_of(mut path: ObjectPath) -> ObjectPath {
    while matches!(
        path.segments.last().map(|s| s.kind),
        Some(ObjectKind::Table | ObjectKind::View | ObjectKind::Folder)
    ) {
        path.segments.pop();
    }
    path
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
    let dname = dialect_name(sd)?;
    let src = export_source(database, targets)?;
    let objects = list_objects(session.as_ref(), &src).await;
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
        "formatVersion": 2,
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
        "objects": objects,
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
struct NullSink;

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

/// Statements that empty the target before the script runs (`clean`).
fn clean_statements(pre: &Json, sd: ScriptDialect, database: &str) -> Vec<String> {
    let q = dbstudio_sql::Dialect::new(sd);
    match sd {
        ScriptDialect::Postgres => {
            let mut schemas: Vec<String> = pre["objects"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| o["name"].as_str())
                .filter_map(|n| n.split_once('.').map(|(s, _)| s.to_string()))
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
        ScriptDialect::MySql { .. } => vec![
            format!("DROP DATABASE IF EXISTS {}", q.quote_ident(database)),
            format!("CREATE DATABASE {}", q.quote_ident(database)),
        ],
        _ => Vec::new(),
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
    reporter: &dyn JobReporter,
) -> Result<crate::RestoreOutcome> {
    let pre = &scanned.preamble;
    let src_dialect = pre["dialect"].as_str().unwrap_or("");
    let sd = session.script_dialect();
    if dialect_name(sd).is_err() || !same_family(src_dialect, sd) {
        return Err(Error::Unsupported(format!(
            "this backup was made from {src_dialect}; restore it into a server of the same kind"
        )));
    }
    if !spec.objects.is_empty() {
        return Err(Error::Unsupported(
            "restoring selected objects is not available for integrated backups; restore the whole backup"
                .into(),
        ));
    }
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
    // MySQL `clean` replaces the whole database: do it on the session, before the lease opens.
    let clean = if spec.clean {
        clean_statements(pre, sd, &target_db)
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
        pre,
        &clean,
        dump_path,
        spec,
        single_tx,
        &mut progress,
        reporter,
    )
    .await;
    target.close().await;
    let errors = result?;
    let rows = scanned
        .manifest
        .entries
        .iter()
        .find(|e| e.path == dump_name)
        .map_or(0, |e| e.rows);
    Ok(crate::RestoreOutcome {
        objects: pre["tables"].as_u64().unwrap_or(0) as u32,
        rows: if errors.is_empty() { rows } else { 0 },
        errors,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_script(
    target: &Target,
    cancel: &CancelToken,
    sd: ScriptDialect,
    pre: &Json,
    clean: &[String],
    path: &Path,
    spec: &crate::RestoreSpec,
    single_tx: bool,
    progress: &mut JobProgress,
    reporter: &dyn JobReporter,
) -> Result<Vec<RowError>> {
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
    let _ = pre;
    let file =
        File::open(path).map_err(|e| Error::Other(format!("cannot read the script: {e}")))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    progress.bytes_total = Some(size);
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let mut splitter = StreamSplitter::new(script_dialect_of(sd));
    let mut errors: Vec<RowError> = Vec::new();
    let (mut n, mut bytes) = (0u64, 0u64);
    let mut line = String::new();
    let mut eof = false;
    while !eof {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| Error::Other(format!("cannot read the script: {e}")))?;
        bytes += read as u64;
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
            n += 1;
            match target.run(cancel, stmt).await {
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
    Ok(errors)
}

fn script_dialect_of(sd: ScriptDialect) -> ScriptDialect {
    match sd {
        ScriptDialect::MySql { mariadb, .. } => ScriptDialect::MySql {
            mariadb,
            backslash_escapes: true,
        },
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
        assert_eq!(scope_of(t).segments.len(), 1);
    }

    #[test]
    fn clean_replaces_schemas_or_the_database() {
        let pre =
            json!({"objects": [{"name": "app.users"}, {"name": "app.orders"}, {"name": "x.t"}]});
        let pg = clean_statements(&pre, ScriptDialect::Postgres, "d");
        assert_eq!(pg[0], "DROP SCHEMA IF EXISTS \"app\" CASCADE");
        assert_eq!(pg.len(), 2);
        let my = clean_statements(
            &pre,
            ScriptDialect::MySql {
                mariadb: false,
                backslash_escapes: true,
            },
            "d",
        );
        assert_eq!(my, ["DROP DATABASE IF EXISTS `d`", "CREATE DATABASE `d`"]);
        assert_eq!(
            clean_statements(&json!({}), ScriptDialect::Postgres, "d"),
            [
                "DROP SCHEMA IF EXISTS \"public\" CASCADE",
                "CREATE SCHEMA \"public\""
            ]
        );
    }

    #[test]
    fn dialect_families() {
        assert!(same_family("postgres", ScriptDialect::Postgres));
        assert!(!same_family("postgres", ScriptDialect::MsSql));
        assert_eq!(dialect_name(ScriptDialect::Oracle).unwrap(), "oracle");
    }
}
