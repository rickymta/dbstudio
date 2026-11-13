//! Backup, restore, inspect, verify and retention.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Local, Utc};
use dbstudio_core::{
    CancelToken, Error, JobProgress, JobReporter, ObjectKind, ObjectPath, QueryRequest, Result,
    Secret, Session,
};
use dbstudio_export::ExportContent;
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};

use crate::container::{self, ContainerOptions, SpoolEntry};
use crate::crypto::hex;
use crate::{
    BackupContext, BackupEngine, BackupInspection, BackupJob, BackupOutcome, BackupRun, BackupSpec,
    Destination, NativeTool, RestoreOutcome, RestoreSpec, RunKind, RunStatus, logical, native,
};

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Run blocking `work` on the blocking pool, forwarding the byte counts it reports to the
/// reporter and honouring cancellation.
async fn blocking_with_progress<T, F>(
    reporter: &dyn JobReporter,
    mut progress: JobProgress,
    work: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&dyn Fn(u64) -> bool) -> Result<T> + Send + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
    let cancel: CancelToken = reporter.cancel_token().clone();
    let mut handle = tokio::task::spawn_blocking(move || {
        let tick = move |bytes: u64| {
            let _ = tx.send(bytes);
            !cancel.is_cancelled()
        };
        work(&tick)
    });
    loop {
        tokio::select! {
            Some(bytes) = rx.recv() => {
                progress.bytes_done += bytes;
                reporter.progress(&progress);
            }
            res = &mut handle => {
                return res.map_err(|e| Error::Other(format!("background task failed: {e}")))?;
            }
        }
    }
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub(crate) fn spool_entry_from_file(name: &str, path: &Path) -> Result<SpoolEntry> {
    Ok(SpoolEntry {
        name: name.to_string(),
        path: path.to_path_buf(),
        sha256: file_sha256(path)?,
        bytes: fs::metadata(path)?.len(),
        rows: 0,
    })
}

fn clean_component(v: &str) -> String {
    let s: String = v
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    s.trim_matches('.').to_string()
}

/// Expand a file name template: `{connection}`, `{database}`, `{job}` and
/// `{date:%Y%m%d-%H%M%S}` (chrono format, local time). Substituted values are sanitized and
/// the result always ends in `.dbsbak` and contains no path separator.
pub fn render_file_name(
    template: &str,
    connection: &str,
    database: &str,
    job: &str,
    now: DateTime<Local>,
) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let key = &after[..end];
        match key {
            "connection" => out.push_str(&clean_component(connection)),
            "database" => out.push_str(&clean_component(database)),
            "job" => out.push_str(&clean_component(job)),
            "date" => out.push_str(&now.format("%Y%m%d-%H%M%S").to_string()),
            k if k.starts_with("date:") => {
                let fmt = &k[5..];
                // An invalid chrono format would panic while formatting; check first.
                let items: Vec<_> = chrono::format::StrftimeItems::new(fmt).collect();
                if items
                    .iter()
                    .any(|i| matches!(i, chrono::format::Item::Error))
                {
                    out.push_str(&now.format("%Y%m%d-%H%M%S").to_string());
                } else {
                    out.push_str(&clean_component(&now.format(fmt).to_string()));
                }
            }
            _ => {
                out.push('{');
                out.push_str(key);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    let mut name: String = out
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect();
    if name.trim().is_empty() || name == ".dbsbak" {
        name = format!("backup-{}", now.format("%Y%m%d-%H%M%S"));
    }
    if !name.ends_with(".dbsbak") {
        name.push_str(".dbsbak");
    }
    name
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let stem = name.trim_end_matches(".dbsbak");
    (1..)
        .map(|i| dir.join(format!("{stem}-{i}.dbsbak")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Databases to back up: grouped by the database in each target (all of `config.database`
/// when there is no target).
fn groups(ctx: &BackupContext, spec: &BackupSpec) -> Result<Vec<(String, Vec<ObjectPath>)>> {
    let mut out: Vec<(String, Vec<ObjectPath>)> = Vec::new();
    for t in &spec.targets {
        let db = logical::database_of(t)
            .ok_or_else(|| Error::Other("a backup target must be inside a database".into()))?;
        match out.iter_mut().find(|(d, _)| *d == db) {
            Some((_, v)) => v.push(t.clone()),
            None => out.push((db, vec![t.clone()])),
        }
    }
    if out.is_empty() {
        let db = ctx.config.database.clone().unwrap_or_default();
        let path = if db.is_empty() {
            ObjectPath::root()
        } else {
            ObjectPath::root().child(ObjectKind::Database, db.clone())
        };
        out.push((db, vec![path]));
    }
    Ok(out)
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn source_json(ctx: &BackupContext, version: &str, database: &str) -> Json {
    json!({
        "kind": ctx.config.kind.id(),
        "host": ctx.config.host,
        "database": database,
        "serverVersion": version,
    })
}

// ---------------------------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------------------------

/// Run a backup.
pub async fn run_backup(
    ctx: BackupContext,
    spec: &BackupSpec,
    passphrase: Option<&Secret>,
) -> Result<BackupOutcome> {
    let reporter = ctx.reporter.clone();
    let (dir, template) = match &spec.destination {
        Destination::LocalDir {
            path,
            file_template,
        } => (PathBuf::from(path), file_template.clone()),
        Destination::ServerDir { path } => {
            return match &spec.engine {
                BackupEngine::Native {
                    tool: NativeTool::MsSqlServerBackup,
                    ..
                } => mssql_server_backup(&ctx, spec, path).await,
                _ => Err(Error::Unsupported(
                    "a server directory destination needs the SQL Server native backup".into(),
                )),
            };
        }
    };
    let passphrase = if spec.encrypted {
        Some(passphrase.ok_or_else(|| {
            Error::Other("this backup is encrypted but no passphrase is available".into())
        })?)
    } else {
        None
    };
    fs::create_dir_all(&dir)?;
    let server_version = ctx
        .session
        .server_info()
        .await
        .map(|i| i.version)
        .unwrap_or_default();
    let mut outcome = BackupOutcome {
        files: Vec::new(),
        size_bytes: 0,
        checksum: String::new(),
        objects: 0,
        rows: 0,
    };
    for (database, targets) in groups(&ctx, spec)? {
        if reporter.cancel_token().is_cancelled() {
            return Err(Error::Cancelled);
        }
        let spool = tempfile::Builder::new()
            .prefix(".dbstudio-spool-")
            .tempdir_in(&dir)?;
        let source = source_json(&ctx, &server_version, &database);
        let (mut entries, mut preamble, objects, rows, native_tool) = match &spec.engine {
            BackupEngine::Logical { content, ddl } => {
                let d = logical::dump(
                    &ctx.session,
                    ctx.config.kind,
                    spool.path(),
                    &database,
                    &targets,
                    *content,
                    ddl,
                    source,
                    &reporter,
                )
                .await?;
                (d.entries, d.preamble, d.objects, d.rows, None)
            }
            BackupEngine::Native { tool, options } => {
                let (entry, pre) = native_dump(
                    &ctx,
                    *tool,
                    options,
                    &database,
                    spool.path(),
                    &server_version,
                    source,
                )
                .await?;
                (vec![entry], pre, 1, 0, Some(*tool))
            }
        };
        let now = Local::now();
        let name = render_file_name(
            &template,
            &ctx.config.name,
            if database.is_empty() {
                "all"
            } else {
                &database
            },
            ctx.job_name.as_deref().unwrap_or("manual"),
            now,
        );
        let dest = unique_path(&dir, &name);
        let opts = ContainerOptions {
            compression: spec.compression,
            passphrase: passphrase.cloned(),
            native: native_tool,
            created_at: now_rfc3339(),
            kdf_cost: None,
        };
        let phase = if passphrase.is_some() {
            "encrypt"
        } else {
            "compress"
        };
        let progress = JobProgress {
            phase: phase.into(),
            bytes_total: Some(entries.iter().map(|e| e.bytes).sum()),
            objects_done: objects,
            objects_total: Some(objects),
            rows_done: rows,
            ..Default::default()
        };
        let spool_path = spool.path().to_path_buf();
        let dest2 = dest.clone();
        let size = blocking_with_progress(reporter.as_ref(), progress, move |tick| {
            container::write_container(
                &dest2,
                &opts,
                &preamble_take(&mut preamble),
                &std::mem::take(&mut entries),
                &spool_path,
                tick,
            )
        })
        .await?;
        let sum = file_sha256(&dest)?;
        if outcome.files.is_empty() {
            outcome.checksum = sum;
        }
        outcome.files.push(dest.to_string_lossy().into_owned());
        outcome.size_bytes += size;
        outcome.objects += objects;
        outcome.rows += rows;
        reporter.log(&format!("wrote {} ({size} bytes)", dest.display()));
    }
    Ok(outcome)
}

fn preamble_take(p: &mut Json) -> Json {
    std::mem::take(p)
}

async fn native_dump(
    ctx: &BackupContext,
    tool: NativeTool,
    options: &std::collections::BTreeMap<String, String>,
    database: &str,
    spool: &Path,
    server_version: &str,
    source: Json,
) -> Result<(SpoolEntry, Json)> {
    if tool == NativeTool::MsSqlServerBackup {
        return Err(Error::Unsupported(
            "the SQL Server backup writes on the server: choose a server directory".into(),
        ));
    }
    if matches!(tool, NativeTool::Expdp | NativeTool::Impdp) {
        return Err(Error::Unsupported(
            "Oracle Data Pump needs a DIRECTORY object on the server and is not available yet"
                .into(),
        ));
    }
    let info = native::require_tool(tool, &ctx.tool_overrides)?;
    if tool == NativeTool::PgDump {
        native::check_pg_version(&info, server_version)?;
    }
    let out = spool.join("native-dump");
    let cmd = native::build_dump(
        tool,
        &info,
        &ctx.config,
        ctx.secret.as_ref(),
        ctx.tunnel_addr,
        database,
        options,
        spool,
        &out,
    )?;
    let phase = JobProgress {
        phase: "data".into(),
        object: Some(database.to_string()),
        message: Some(format!("{} running", tool.binary().unwrap_or("tool"))),
        ..Default::default()
    };
    ctx.reporter.progress(&phase);
    let (guards, extension, format) = (cmd.guards, cmd.extension, cmd.format);
    native::run_tool(cmd.run, ctx.reporter.as_ref()).await?;
    drop(guards);
    let name = format!("native/dump.{extension}");
    let entry = spool_entry_from_file(&name, &out)?;
    let preamble = json!({
        "formatVersion": 1,
        "engine": "native",
        "createdAt": now_rfc3339(),
        "database": database,
        "source": source,
        "native": {"tool": tool, "format": format, "version": info.version, "file": name},
        "options": options,
        "objects": [{"name": database, "kind": "database"}],
        "unsupported": [],
    });
    Ok((entry, preamble))
}

async fn mssql_server_backup(
    ctx: &BackupContext,
    spec: &BackupSpec,
    server_dir: &str,
) -> Result<BackupOutcome> {
    let mut files = Vec::new();
    for (database, _) in groups(ctx, spec)? {
        let name = render_file_name(
            "{database}-{date:%Y%m%d-%H%M%S}",
            &ctx.config.name,
            &database,
            ctx.job_name.as_deref().unwrap_or("manual"),
            Local::now(),
        )
        .replace(".dbsbak", ".bak");
        let sep = if server_dir.contains('\\') { "\\" } else { "/" };
        let target = format!("{}{sep}{name}", server_dir.trim_end_matches(['/', '\\']));
        let sql = format!(
            "BACKUP DATABASE [{}] TO DISK = N'{}' WITH COPY_ONLY, COMPRESSION, CHECKSUM, INIT",
            database.replace(']', "]]"),
            target.replace('\'', "''")
        );
        let mut req = QueryRequest::new(sql);
        req.database = Some("master".into());
        dbstudio_core::execute_collect(ctx.session.as_ref(), req).await?;
        ctx.reporter.log(&format!("server wrote {target}"));
        files.push(target);
    }
    Ok(BackupOutcome {
        objects: files.len() as u32,
        files,
        size_bytes: 0,
        checksum: String::new(),
        rows: 0,
    })
}

// ---------------------------------------------------------------------------------------------
// Inspect / verify
// ---------------------------------------------------------------------------------------------

/// Read the header (and preamble when possible).
pub async fn inspect(path: &Path, passphrase: Option<&Secret>) -> Result<BackupInspection> {
    let path = path.to_path_buf();
    let pass = passphrase.cloned();
    tokio::task::spawn_blocking(move || {
        let header = container::header_of(&path)?;
        let encrypted = header.encryption.is_some();
        let preamble = if encrypted && pass.is_none() {
            None
        } else if let Some(s) = header.summary.clone() {
            Some(s)
        } else {
            Some(container::read_preamble(&path, pass.as_ref())?)
        };
        let native = header.native;
        let mut header_json =
            serde_json::to_value(&header).map_err(|e| Error::Other(e.to_string()))?;
        // The summary is returned as `preamble`; do not send it twice.
        if let Some(o) = header_json.as_object_mut() {
            o.remove("summary");
        }
        Ok(BackupInspection {
            header: header_json,
            preamble,
            encrypted,
            native,
        })
    })
    .await
    .map_err(|e| Error::Other(format!("background task failed: {e}")))?
}

/// Check integrity of the whole file.
pub async fn verify(
    path: &Path,
    passphrase: Option<&Secret>,
    reporter: &dyn JobReporter,
) -> Result<()> {
    let path = path.to_path_buf();
    let pass = passphrase.cloned();
    let total = fs::metadata(&path)?.len();
    let progress = JobProgress {
        phase: "verify".into(),
        bytes_total: Some(total),
        ..Default::default()
    };
    reporter.progress(&progress);
    blocking_with_progress(reporter, progress, move |tick| {
        container::scan(&path, pass.as_ref(), None, tick).map(|_| ())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------------------------

/// Restore a backup.
pub async fn run_restore(
    ctx: BackupContext,
    spec: &RestoreSpec,
    passphrase: Option<&Secret>,
) -> Result<RestoreOutcome> {
    let reporter = ctx.reporter.clone();
    let file = PathBuf::from(&spec.file);
    let work = tempfile::tempdir()?;
    // The whole file is read and checked before anything touches the target.
    let progress = JobProgress {
        phase: "verify".into(),
        bytes_total: fs::metadata(&file).ok().map(|m| m.len()),
        ..Default::default()
    };
    let (f2, p2, w2) = (file.clone(), passphrase.cloned(), work.path().to_path_buf());
    let scanned = blocking_with_progress(reporter.as_ref(), progress, move |tick| {
        container::scan(&f2, p2.as_ref(), Some(&w2), tick)
    })
    .await?;
    match scanned.header.native {
        None => logical::restore(&ctx.session, &scanned, spec, reporter.as_ref()).await,
        Some(tool) => native_restore(&ctx, &scanned, tool, spec, work.path()).await,
    }
}

async fn native_restore(
    ctx: &BackupContext,
    scanned: &container::Scanned,
    tool: NativeTool,
    spec: &RestoreSpec,
    tmp: &Path,
) -> Result<RestoreOutcome> {
    let pre = &scanned.preamble;
    let format = pre["native"]["format"].as_str().unwrap_or("plain");
    let restore_tool = native::restore_tool_for(tool, format)?;
    let info = native::require_tool(restore_tool, &ctx.tool_overrides)?;
    let target_db = spec
        .target_database
        .clone()
        .or_else(|| pre["database"].as_str().map(str::to_string))
        .ok_or_else(|| Error::Other("target database is not specified".into()))?;
    if spec.create_database {
        let sql = match ctx.config.kind {
            dbstudio_core::DbKind::Postgres => {
                format!("CREATE DATABASE \"{}\"", target_db.replace('"', "\"\""))
            }
            dbstudio_core::DbKind::MySql => format!(
                "CREATE DATABASE IF NOT EXISTS `{}`",
                target_db.replace('`', "``")
            ),
            _ => String::new(),
        };
        if !sql.is_empty() {
            let mut req = QueryRequest::new(sql);
            if ctx.config.kind == dbstudio_core::DbKind::Postgres {
                req.database = Some("postgres".into());
            }
            if let Err(e) = ctx.session.execute(req, &mut NullSink).await {
                if !e.message().contains("already exists") {
                    return Err(e);
                }
            }
        }
    }
    let dump_name = pre["native"]["file"].as_str().unwrap_or_default();
    let input = scanned
        .files
        .get(dump_name)
        .ok_or_else(|| Error::Integrity("native dump missing from the backup".into()))?;
    let cmd = native::build_restore(
        restore_tool,
        format,
        &info,
        &ctx.config,
        ctx.secret.as_ref(),
        ctx.tunnel_addr,
        &target_db,
        spec.clean,
        spec.continue_on_error,
        tmp,
        input,
    )?;
    ctx.reporter.progress(&JobProgress {
        phase: "restore".into(),
        object: Some(target_db),
        ..Default::default()
    });
    let guards = cmd.guards;
    let tail = native::run_tool(cmd.run, ctx.reporter.as_ref()).await?;
    drop(guards);
    let errors = if spec.continue_on_error {
        tail.lines()
            .filter(|l| l.contains("ERROR") || l.contains("error:"))
            .enumerate()
            .map(|(i, l)| dbstudio_export::RowError {
                line: i as u64 + 1,
                message: l.trim().to_string(),
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(RestoreOutcome {
        objects: 1,
        rows: 0,
        errors,
    })
}

struct NullSink;

impl dbstudio_core::ResultSink for NullSink {
    fn on_columns(&mut self, _: Vec<dbstudio_core::ColumnMeta>) -> Result<()> {
        Ok(())
    }
    fn on_rows(&mut self, _: Vec<Vec<dbstudio_core::Value>>) -> Result<()> {
        Ok(())
    }
    fn on_message(&mut self, _: String) -> Result<()> {
        Ok(())
    }
    fn on_statement_end(&mut self, _: dbstudio_core::StatementEnd) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------------------------

/// Files of `job` that retention says to delete. Only files recorded as successful backup runs
/// of this job are considered (no globbing); the newest file is always kept.
pub fn apply_retention(job: &BackupJob, runs: &[BackupRun], now: DateTime<Utc>) -> Vec<PathBuf> {
    let mut files: Vec<(DateTime<Utc>, PathBuf)> = runs
        .iter()
        .filter(|r| {
            r.job_id == Some(job.id)
                && r.kind == RunKind::Backup
                && r.status == RunStatus::Succeeded
        })
        .filter_map(|r| {
            let at = DateTime::parse_from_rfc3339(&r.started_at)
                .ok()?
                .with_timezone(&Utc);
            Some((at, PathBuf::from(r.file_path.as_ref()?)))
        })
        .collect();
    files.sort_by_key(|a| std::cmp::Reverse(a.0));
    let mut doomed = BTreeSet::new();
    if let Some(keep) = job.retention.keep_last {
        for (_, p) in files.iter().skip(keep.max(1) as usize) {
            doomed.insert(p.clone());
        }
    }
    if let Some(days) = job.retention.max_age_days {
        let limit = now - Duration::days(i64::from(days));
        for (at, p) in files.iter().skip(1) {
            if *at < limit {
                doomed.insert(p.clone());
            }
        }
    }
    // A path shared with a kept run (same file recorded twice) is never deleted.
    let kept: BTreeSet<&PathBuf> = files
        .iter()
        .map(|(_, p)| p)
        .filter(|p| !doomed.contains(*p))
        .collect();
    let _ = kept;
    files
        .into_iter()
        .map(|(_, p)| p)
        .filter(|p| doomed.contains(p))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[allow(dead_code)]
fn _unused(_: &dyn Session, _: ExportContent) {}
