//! Integrated ("logical") backup of MongoDB and Redis through the export / import engine.
//!
//! * MongoDB (`engine: "logical-mongo"`): per collection `schema/<db>/<n>-<name>.json` (the
//!   statements that re-create the collection with its options and validator, its indexes, or
//!   the view) and `data/<db>/<n>-<name>.ndjson` (canonical Extended JSON, one document per
//!   line, streamed by the export engine inside a snapshot transaction on replica sets).
//!   Restore: create the collection, `bulk_insert` the documents in batches of 1000, create the
//!   indexes, then the views.
//! * Redis (`engine: "logical-redis"`): `data/db<N>.ndjson`, one `KeyRecord` per line. Restore
//!   writes them back with `KeyChange::Put`; keys that expired meanwhile are skipped.

use std::path::Path;
use std::sync::Arc;

use dbstudio_core::{
    DbKind, DdlRequest, DdlSection, Error, JobProgress, JobReporter, ObjectKind, ObjectPath,
    PutMode, QueryRequest, Result, ScriptDialect, Session, folder,
};
use dbstudio_export::{
    ExportContent, ExportContext, ExportDestination, ExportFormat, ExportSource, ExportSpec,
    ExtendedJson, ImportFormat, ImportMode, ImportSpec, ImportTarget, JsonLayout, JsonOptions,
    RowError, ValueFormat, run_export, run_import,
};
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::container::Scanned;
use crate::logical::{Dump, NullSink};
use crate::{RestoreOutcome, RestoreSpec};

/// What neither engine contains.
const MONGO_UNSUPPORTED: [&str; 4] = [
    "users and roles",
    "other databases of the server",
    "the oplog and sharding metadata",
    "a snapshot across collections (each collection is read in its own snapshot)",
];
const REDIS_UNSUPPORTED: [&str; 4] = [
    "other databases of the server",
    "stream consumer groups",
    "functions and modules",
    "a snapshot (keys written while the dump runs may be mixed)",
];

/// Documents per `bulk_insert` call is fixed by the import engine (1000).
const MAX_RESTORE_ERRORS: u32 = 1000;

fn now_text() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A name usable inside a tar entry: ASCII letters, digits, `-` and `_` stay, the rest is
/// percent-encoded, long names are cut (the numeric prefix keeps entries unique).
fn encode_name(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
        if out.len() >= 80 {
            break;
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

fn ctx_for(
    session: &Arc<dyn Session>,
    kind: DbKind,
    dialect: ScriptDialect,
    reporter: &Arc<dyn JobReporter>,
) -> ExportContext {
    ExportContext {
        session: Arc::clone(session),
        kind,
        dialect,
        reporter: Arc::clone(reporter),
    }
}

fn ndjson_format() -> ExportFormat {
    ExportFormat::Json(JsonOptions {
        layout: JsonLayout::Ndjson,
        extended_json: ExtendedJson::Canonical,
        pretty: false,
        fields: Vec::new(),
    })
}

// ---------------------------------------------------------------------------------------------
// MongoDB
// ---------------------------------------------------------------------------------------------

fn db_path(database: &str) -> ObjectPath {
    ObjectPath::root().child(ObjectKind::Database, database)
}

fn coll_path(database: &str, name: &str, view: bool) -> ObjectPath {
    db_path(database)
        .child(
            ObjectKind::Folder,
            if view {
                folder::VIEWS
            } else {
                folder::COLLECTIONS
            },
        )
        .child(
            if view {
                ObjectKind::View
            } else {
                ObjectKind::Collection
            },
            name,
        )
}

/// `(name, is_view)` of every collection and view of `database`.
async fn list_collections(session: &dyn Session, database: &str) -> Result<Vec<(String, bool)>> {
    let mut out = Vec::new();
    for f in session.list_children(&db_path(database)).await? {
        if f.kind != ObjectKind::Folder {
            continue;
        }
        for c in session.list_children(&f.path).await? {
            match c.kind {
                ObjectKind::Collection => out.push((c.name, false)),
                ObjectKind::View => out.push((c.name, true)),
                _ => {}
            }
        }
    }
    Ok(out)
}

/// Collections chosen by `targets` (empty or a database node = all).
fn choose(all: Vec<(String, bool)>, targets: &[ObjectPath]) -> Vec<(String, bool)> {
    let names: Vec<&str> = targets
        .iter()
        .filter(|t| {
            t.segments
                .iter()
                .rfind(|s| s.kind != ObjectKind::Folder)
                .is_some_and(|s| matches!(s.kind, ObjectKind::Collection | ObjectKind::View))
        })
        .filter_map(|t| t.segments.last().map(|s| s.name.as_str()))
        .collect();
    let whole = targets.is_empty()
        || targets.iter().any(|t| {
            t.segments
                .last()
                .is_some_and(|s| s.kind == ObjectKind::Database)
        });
    all.into_iter()
        .filter(|(n, _)| whole || names.contains(&n.as_str()))
        .collect()
}

/// Back up the collections of one MongoDB database.
pub(crate) async fn dump_mongo(
    session: &Arc<dyn Session>,
    spool: &Path,
    database: &str,
    targets: &[ObjectPath],
    content: ExportContent,
    source: Json,
    reporter: &Arc<dyn JobReporter>,
) -> Result<Dump> {
    if database.is_empty() {
        return Err(Error::Invalid(
            "choose the MongoDB database to back up".into(),
        ));
    }
    let mut colls = choose(list_collections(session.as_ref(), database).await?, targets);
    // Collections first: a view reads from them.
    colls.sort_by_key(|(_, view)| *view);
    let dbenc = encode_name(database);
    let mut entries = Vec::new();
    let mut objects: Vec<Json> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let (mut total_rows, mut index) = (0u64, 0u32);
    let mut progress = JobProgress {
        phase: "data".into(),
        objects_total: Some(colls.len() as u32),
        ..Default::default()
    };
    for (name, view) in &colls {
        if reporter.cancel_token().is_cancelled() {
            return Err(Error::Cancelled);
        }
        index += 1;
        let enc = encode_name(name);
        let (mut has_pre, mut has_data, mut has_post) = (false, false, false);
        let mut obj = json!({
            "kind": if *view { "view" } else { "collection" },
            "name": name,
        });
        progress.object = Some(name.clone());
        reporter.progress(&progress);
        if content != ExportContent::Data {
            let items = session
                .generate_ddl(DdlRequest {
                    scope: db_path(database),
                    objects: vec![coll_path(database, name, *view)],
                    options: dbstudio_core::DdlOptions::default(),
                })
                .await?;
            let (mut pre, mut post) = (Vec::new(), Vec::new());
            for it in items {
                if it.section == DdlSection::PreData {
                    pre.push(it.sql);
                } else {
                    post.push(it.sql);
                }
            }
            let entry_name = format!("schema/{dbenc}/{index:04}-{enc}.json");
            let file = spool.join(format!("schema-{index:04}.json"));
            let body = json!({"kind": obj["kind"], "name": name, "pre": pre, "post": post});
            std::fs::write(
                &file,
                serde_json::to_vec(&body).map_err(|e| Error::Other(e.to_string()))?,
            )?;
            entries.push(crate::ops::spool_entry_from_file(&entry_name, &file)?);
            obj["schemaFile"] = json!(entry_name);
            has_pre = true;
            has_post = !post.is_empty();
        }
        if content != ExportContent::Schema && !*view {
            let entry_name = format!("data/{dbenc}/{index:04}-{enc}.ndjson");
            let file = spool.join(format!("data-{index:04}.ndjson"));
            let spec = ExportSpec {
                connection_id: Uuid::nil(),
                source: ExportSource::Tables {
                    paths: vec![coll_path(database, name, false)],
                },
                content: ExportContent::Data,
                format: ndjson_format(),
                destination: ExportDestination {
                    path: file.to_string_lossy().into_owned(),
                    overwrite: true,
                },
                values: ValueFormat::default(),
            };
            let report = run_export(
                ctx_for(
                    session,
                    DbKind::MongoDb,
                    ScriptDialect::MongoShell,
                    reporter,
                ),
                spec,
            )
            .await?;
            for w in report.warnings {
                warnings.push(format!("{name}: {w}"));
            }
            // An empty collection may write no file at all.
            if !file.exists() {
                std::fs::write(&file, b"")?;
            }
            let mut entry = crate::ops::spool_entry_from_file(&entry_name, &file)?;
            entry.rows = report.rows;
            entries.push(entry);
            total_rows += report.rows;
            obj["dataFile"] = json!(entry_name);
            obj["rows"] = json!(report.rows);
            has_data = true;
        }
        // Data sits between the structure and the indexes.
        let phases: Vec<&str> = [(has_pre, "pre"), (has_data, "data"), (has_post, "post")]
            .into_iter()
            .filter_map(|(on, p)| on.then_some(p))
            .collect();
        obj["phases"] = json!(phases);
        objects.push(obj);
        progress.objects_done += 1;
        progress.rows_done = total_rows;
    }
    for w in &warnings {
        reporter.log(w);
    }
    let preamble = json!({
        "formatVersion": 3,
        "engine": "logical-mongo",
        "createdAt": now_text(),
        "database": database,
        "source": source,
        "dialect": "mongo",
        "content": match content {
            ExportContent::Schema => "schema",
            ExportContent::Data => "data",
            ExportContent::Both => "both",
        },
        "objects": objects,
        "tables": colls.len(),
        "warnings": warnings,
        "unsupported": MONGO_UNSUPPORTED,
    });
    Ok(Dump {
        objects: colls.len() as u32,
        rows: total_rows,
        entries,
        preamble,
    })
}

/// Names of the collections a restore of `pre` covers, in restore order (collections, views).
fn restore_plan(pre: &Json, selectors: &[String]) -> Result<Vec<Json>> {
    let all: Vec<Json> = pre["objects"].as_array().cloned().unwrap_or_default();
    if selectors.is_empty() {
        return Ok(all);
    }
    let mut out = Vec::new();
    for sel in selectors {
        match all.iter().find(|o| o["name"].as_str() == Some(sel)) {
            Some(o) => out.push(o.clone()),
            None => {
                return Err(Error::Invalid(format!(
                    "the collection {sel:?} is not in this backup"
                )));
            }
        }
    }
    // Keep the order of the backup (collections before views).
    out.sort_by_key(|o| {
        all.iter()
            .position(|a| a["name"] == o["name"])
            .unwrap_or(usize::MAX)
    });
    out.dedup_by(|a, b| a["name"] == b["name"]);
    Ok(out)
}

async fn run_statement(session: &Arc<dyn Session>, database: &str, text: String) -> Result<()> {
    let mut req = QueryRequest::new(text);
    req.database = Some(database.to_string());
    session.execute(req, &mut NullSink).await.map(|_| ())
}

fn shell_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Run the `createCollection` statements of `schema` on a scratch collection and drop it again.
async fn rehearse(
    session: &Arc<dyn Session>,
    database: &str,
    name: &str,
    schema: &Json,
) -> Result<()> {
    let tmp = format!(
        "__dbstudio_try_{}",
        &Uuid::new_v4().simple().to_string()[..8]
    );
    let head = format!("db.createCollection({}", shell_str(name));
    for stmt in schema["pre"].as_array().into_iter().flatten() {
        let Some(text) = stmt.as_str() else { continue };
        // Views and anything else that does not start like that need no rehearsal.
        let Some(rest) = text.strip_prefix(&head) else {
            continue;
        };
        let attempt = format!("db.createCollection({}{rest}", shell_str(&tmp));
        let result = run_statement(session, database, attempt).await;
        let _ = run_statement(
            session,
            database,
            format!("db.getCollection({}).drop();", shell_str(&tmp)),
        )
        .await;
        result?;
    }
    Ok(())
}

fn read_json(path: &Path) -> Result<Json> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| Error::Integrity(format!("bad schema file: {e}")))
}

/// Restore a MongoDB backup (all collections, or the chosen ones).
pub(crate) async fn restore_mongo(
    session: &Arc<dyn Session>,
    scanned: &Scanned,
    spec: &RestoreSpec,
    reporter: &Arc<dyn JobReporter>,
) -> Result<RestoreOutcome> {
    let pre = &scanned.preamble;
    if !matches!(session.script_dialect(), ScriptDialect::MongoShell) {
        return Err(Error::Unsupported(
            "this backup was made from MongoDB; restore it into a MongoDB server".into(),
        ));
    }
    let target = spec
        .target_database
        .clone()
        .or_else(|| pre["database"].as_str().map(str::to_string))
        .ok_or_else(|| Error::Invalid("target database is not specified".into()))?;
    let plan = restore_plan(pre, &spec.objects)?;
    if spec.clean {
        if let Some(o) = plan.iter().find(|o| o["schemaFile"].is_null()) {
            // A data-only backup cannot re-create what `clean` would drop.
            return Err(Error::Invalid(format!(
                "this backup holds only data (no structure of {}): a clean restore would lose \
                 the collection's options and indexes",
                o["name"].as_str().unwrap_or_default()
            )));
        }
    }
    let cancel = reporter.cancel_token().clone();
    let mut outcome = RestoreOutcome {
        objects: 0,
        rows: 0,
        errors: Vec::new(),
    };
    let mut progress = JobProgress {
        phase: "restore".into(),
        objects_total: Some(plan.len() as u32),
        ..Default::default()
    };
    let lenient = spec.continue_on_error;
    let note = |outcome: &mut RestoreOutcome, name: &str, e: Error| -> Result<()> {
        if lenient && !matches!(e, Error::Cancelled) {
            outcome.errors.push(RowError {
                line: (outcome.errors.len() + 1) as u64,
                message: format!("{name}: {}", e.message()),
            });
            Ok(())
        } else {
            Err(e)
        }
    };
    for o in &plan {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let name = o["name"].as_str().unwrap_or_default();
        let view = o["kind"] == "view";
        progress.object = Some(name.to_string());
        reporter.progress(&progress);
        let schema = o["schemaFile"]
            .as_str()
            .and_then(|f| scanned.files.get(f))
            .map(|p| read_json(p))
            .transpose()?;
        if spec.clean {
            // The structure statements are tried on a scratch name first: when they cannot run
            // on this server the existing collection is still there.
            if let Some(schema) = &schema {
                if let Err(e) = rehearse(session, &target, name, schema).await {
                    note(&mut outcome, name, e)?;
                    continue;
                }
            }
            // The collection (or view) is replaced; the drop is a no-op when it is absent.
            let drop = format!("db.getCollection({}).drop();", shell_str(name));
            if let Err(e) = run_statement(session, &target, drop).await {
                note(&mut outcome, name, e)?;
                continue;
            }
        }
        if let Some(schema) = &schema {
            for stmt in schema["pre"].as_array().into_iter().flatten() {
                let Some(text) = stmt.as_str() else { continue };
                match run_statement(session, &target, text.to_string()).await {
                    Ok(()) => {}
                    // Restoring into an existing collection adds the documents to it.
                    Err(e) if e.message().contains("already exists") && !spec.clean => {
                        reporter.log(&format!(
                            "{name} already exists: documents are added to it, its options are kept"
                        ));
                    }
                    Err(e) => {
                        note(&mut outcome, name, e)?;
                    }
                }
            }
        }
        let rows = o["rows"].as_u64().unwrap_or(0);
        if let (false, true, Some(file)) = (
            view,
            rows > 0,
            o["dataFile"].as_str().and_then(|f| scanned.files.get(f)),
        ) {
            let import = ImportSpec {
                connection_id: Uuid::nil(),
                file: file.to_string_lossy().into_owned(),
                format: ImportFormat::Json {
                    layout: JsonLayout::Ndjson,
                },
                target: ImportTarget::Existing {
                    table: coll_path(&target, name, false),
                },
                mapping: Vec::new(),
                on_conflict: None,
                mode: ImportMode::Batches { rows: 1000 },
                max_errors: if lenient { MAX_RESTORE_ERRORS } else { 0 },
            };
            let report = run_import(
                ctx_for(
                    session,
                    DbKind::MongoDb,
                    ScriptDialect::MongoShell,
                    reporter,
                ),
                import,
            )
            .await;
            match report {
                Ok(r) => {
                    outcome.rows += r.inserted + r.updated;
                    for e in r.errors {
                        outcome.errors.push(RowError {
                            line: e.line,
                            message: format!("{name}: {}", e.message),
                        });
                    }
                }
                Err(e) => note(&mut outcome, name, e)?,
            }
        }
        if let Some(schema) = &schema {
            for stmt in schema["post"].as_array().into_iter().flatten() {
                let Some(text) = stmt.as_str() else { continue };
                if let Err(e) = run_statement(session, &target, text.to_string()).await {
                    reporter.log(&format!(
                        "the collection {name} has its documents but this index is missing ({}): {text}",
                        e.message()
                    ));
                    note(&mut outcome, name, e)?;
                }
            }
        }
        outcome.objects += 1;
        progress.objects_done += 1;
        progress.rows_done = outcome.rows;
    }
    reporter.progress(&progress);
    Ok(outcome)
}

// ---------------------------------------------------------------------------------------------
// Redis
// ---------------------------------------------------------------------------------------------

/// Back up one Redis database.
pub(crate) async fn dump_redis(
    session: &Arc<dyn Session>,
    spool: &Path,
    database: &str,
    content: ExportContent,
    source: Json,
    reporter: &Arc<dyn JobReporter>,
) -> Result<Dump> {
    if content == ExportContent::Schema {
        return Err(Error::Invalid(
            "Redis keys have no structure: a backup always holds the data".into(),
        ));
    }
    let db = if database.is_empty() { "0" } else { database };
    let entry_name = format!("data/db{}.ndjson", encode_name(db));
    let file = spool.join("keys.ndjson");
    let spec = ExportSpec {
        connection_id: Uuid::nil(),
        source: ExportSource::Keys {
            database: db_path(db),
            pattern: String::new(),
        },
        content: ExportContent::Data,
        format: ExportFormat::Json(JsonOptions {
            layout: JsonLayout::Ndjson,
            extended_json: ExtendedJson::Off,
            pretty: false,
            fields: Vec::new(),
        }),
        destination: ExportDestination {
            path: file.to_string_lossy().into_owned(),
            overwrite: true,
        },
        values: ValueFormat::default(),
    };
    let report = run_export(
        ctx_for(session, DbKind::Redis, ScriptDialect::RedisCli, reporter),
        spec,
    )
    .await?;
    for w in &report.warnings {
        reporter.log(w);
    }
    if !file.exists() {
        std::fs::write(&file, b"")?;
    }
    let mut entry = crate::ops::spool_entry_from_file(&entry_name, &file)?;
    entry.rows = report.rows;
    let mut warnings = report.warnings.clone();
    warnings.push(
        "Redis has no snapshot: keys written while the dump ran may be mixed with older ones"
            .into(),
    );
    let preamble = json!({
        "formatVersion": 3,
        "engine": "logical-redis",
        "createdAt": now_text(),
        "database": db,
        "source": source,
        "dialect": "redis",
        "content": "data",
        "dumpFile": entry_name,
        "objects": [{
            "kind": "database",
            "name": format!("db{db}"),
            "phases": ["data"],
            "rows": report.rows,
        }],
        "tables": 1,
        "warnings": warnings,
        "unsupported": REDIS_UNSUPPORTED,
    });
    Ok(Dump {
        objects: 1,
        rows: report.rows,
        entries: vec![entry],
        preamble,
    })
}

/// Restore a Redis backup into a database of `session`.
///
/// `clean` runs `FLUSHDB` first and overwrites keys (`Replace`); otherwise existing keys are
/// kept (`IfAbsent`).
pub(crate) async fn restore_redis(
    session: &Arc<dyn Session>,
    scanned: &Scanned,
    spec: &RestoreSpec,
    reporter: &Arc<dyn JobReporter>,
) -> Result<RestoreOutcome> {
    let pre = &scanned.preamble;
    if !matches!(session.script_dialect(), ScriptDialect::RedisCli) {
        return Err(Error::Unsupported(
            "this backup was made from Redis; restore it into a Redis server".into(),
        ));
    }
    if !spec.objects.is_empty() {
        return Err(Error::Unsupported(
            "a Redis backup holds one database and is restored whole".into(),
        ));
    }
    let target = spec
        .target_database
        .clone()
        .or_else(|| pre["database"].as_str().map(str::to_string))
        .ok_or_else(|| Error::Invalid("target database is not specified".into()))?;
    let file = pre["dumpFile"]
        .as_str()
        .and_then(|f| scanned.files.get(f))
        .ok_or_else(|| Error::Integrity("the key file is missing from the backup".into()))?;
    reporter.progress(&JobProgress {
        phase: "restore".into(),
        object: Some(format!("db{target}")),
        ..Default::default()
    });
    if spec.clean {
        let mut req = QueryRequest::new("FLUSHDB");
        req.database = Some(target.clone());
        session.execute(req, &mut NullSink).await?;
    }
    let import = ImportSpec {
        connection_id: Uuid::nil(),
        file: file.to_string_lossy().into_owned(),
        format: ImportFormat::Json {
            layout: JsonLayout::Ndjson,
        },
        target: ImportTarget::Keys {
            database: db_path(&target),
            mode: if spec.clean {
                PutMode::Replace
            } else {
                PutMode::IfAbsent
            },
        },
        mapping: Vec::new(),
        on_conflict: None,
        mode: ImportMode::Batches { rows: 200 },
        max_errors: if spec.continue_on_error {
            MAX_RESTORE_ERRORS
        } else {
            0
        },
    };
    let report = run_import(
        ctx_for(session, DbKind::Redis, ScriptDialect::RedisCli, reporter),
        import,
    )
    .await?;
    if report.skipped > 0 {
        reporter.log(&format!(
            "{} key(s) were not written (already present or expired)",
            report.skipped
        ));
    }
    Ok(RestoreOutcome {
        objects: 1,
        rows: report.inserted + report.updated,
        errors: report.errors,
    })
}
