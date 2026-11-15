//! Running saved jobs against the store: run records, per-job file locks, retention and the
//! row <-> typed conversions shared by the desktop app and `dbstudio-cli`.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use dbstudio_core::{
    CancelToken, ConnectionConfig, DriverRegistry, Environment, Error, JobError, JobProgress,
    JobReporter, Result, Secret, Session,
};
use dbstudio_store::{
    BackupJobRow, BackupRunRow, ConnectionService, SecretStore, backup_secret_account,
    ssh_secret_account,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::scheduler::next_after;
use crate::{
    BackupContext, BackupEngine, BackupJob, BackupOutcome, BackupRun, BackupSpec, MissedRunPolicy,
    NativeTool, RestoreOutcome, RestoreSpec, RunKind, RunStatus, RunTrigger, Schedule, ScheduleTz,
    run_backup, run_restore,
};

/// Last bytes of the log kept in `backup_runs.log_tail`.
const LOG_TAIL_BYTES: usize = 16 * 1024;

/// Everything a run needs from the application.
#[derive(Clone)]
pub struct RunEnv {
    /// Profiles, settings and the SQLite store.
    pub service: Arc<ConnectionService>,
    /// Keychain.
    pub secrets: Arc<dyn SecretStore>,
    /// Drivers.
    pub registry: Arc<DriverRegistry>,
    /// Directory of the per-job lock files.
    pub locks_dir: PathBuf,
    /// Application data directory: scratch space (`<work_dir>/tmp`) for plaintext spools.
    pub work_dir: PathBuf,
}

impl std::fmt::Debug for RunEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunEnv")
            .field("locks_dir", &self.locks_dir)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// Row conversions
// ---------------------------------------------------------------------------------------------

fn to_json<T: Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| Error::Storage(format!("cannot serialize: {e}")))
}

fn from_json<T: DeserializeOwned>(text: &str, what: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|e| Error::Storage(format!("corrupt {what}: {e}")))
}

fn enum_text<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|j| j.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn enum_parse<T: DeserializeOwned>(text: &str, what: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(text.to_string()))
        .map_err(|_| Error::Storage(format!("corrupt {what}: {text}")))
}

/// Store row of `job`; `last_scheduled_for` is passed through untouched.
pub fn job_to_row(job: &BackupJob, last_scheduled_for: Option<String>) -> Result<BackupJobRow> {
    Ok(BackupJobRow {
        id: job.id,
        name: job.name.clone(),
        connection_id: job.spec.connection_id,
        enabled: job.enabled,
        spec: to_json(&job.spec)?,
        cron: job.schedule.as_ref().map(|s| s.cron.clone()),
        timezone: job
            .schedule
            .as_ref()
            .map_or("local".to_string(), |s| enum_text(&s.timezone)),
        retention: to_json(&job.retention)?,
        missed_policy: enum_text(&job.missed_run),
        notify: to_json(&job.notify)?,
        encrypted: job.spec.encrypted,
        last_scheduled_for,
        last_run_at: job.last_run_at.clone(),
        next_run_at: job.next_run_at.clone(),
        created_at: job.created_at.clone(),
        updated_at: job.updated_at.clone(),
    })
}

/// Typed job of a store row.
pub fn row_to_job(row: &BackupJobRow) -> Result<BackupJob> {
    let schedule = match &row.cron {
        Some(cron) => Some(Schedule {
            cron: cron.clone(),
            timezone: enum_parse::<ScheduleTz>(&row.timezone, "timezone")?,
        }),
        None => None,
    };
    Ok(BackupJob {
        id: row.id,
        name: row.name.clone(),
        enabled: row.enabled,
        spec: from_json(&row.spec, "backup spec")?,
        schedule,
        retention: from_json(&row.retention, "retention")?,
        missed_run: enum_parse::<MissedRunPolicy>(&row.missed_policy, "missed run policy")?,
        notify: from_json(&row.notify, "notify policy")?,
        last_run_at: row.last_run_at.clone(),
        next_run_at: row.next_run_at.clone(),
        created_at: row.created_at.clone(),
        updated_at: row.updated_at.clone(),
    })
}

/// Store row of a run.
pub fn run_to_row(run: &BackupRun) -> BackupRunRow {
    BackupRunRow {
        id: run.id,
        job_id: run.job_id,
        job_name: run.job_name.clone(),
        connection_id: run.connection_id,
        kind: enum_text(&run.kind),
        trigger: enum_text(&run.trigger),
        engine: run.engine.clone(),
        status: enum_text(&run.status),
        started_at: run.started_at.clone(),
        finished_at: run.finished_at.clone(),
        duration_ms: run.duration_ms,
        file_path: run.file_path.clone(),
        size_bytes: run.size_bytes,
        checksum: run.checksum.clone(),
        objects: run.objects.map(u64::from),
        rows: run.rows,
        error_kind: run.error.as_ref().map(|e| e.kind.clone()),
        error_message: run.error.as_ref().map(|e| e.message.clone()),
        log_tail: run.log_tail.clone(),
    }
}

/// Typed run of a store row.
pub fn row_to_run(row: &BackupRunRow) -> Result<BackupRun> {
    Ok(BackupRun {
        id: row.id,
        job_id: row.job_id,
        job_name: row.job_name.clone(),
        connection_id: row.connection_id,
        kind: enum_parse::<RunKind>(&row.kind, "run kind")?,
        trigger: enum_parse::<RunTrigger>(&row.trigger, "run trigger")?,
        engine: row.engine.clone(),
        status: enum_parse::<RunStatus>(&row.status, "run status")?,
        started_at: row.started_at.clone(),
        finished_at: row.finished_at.clone(),
        duration_ms: row.duration_ms,
        file_path: row.file_path.clone(),
        size_bytes: row.size_bytes,
        checksum: row.checksum.clone(),
        objects: row.objects.map(|o| u32::try_from(o).unwrap_or(u32::MAX)),
        rows: row.rows,
        error: row.error_kind.as_ref().map(|k| JobError {
            kind: k.clone(),
            message: row.error_message.clone().unwrap_or_default(),
        }),
        log_tail: row.log_tail.clone(),
    })
}

/// Name of a run's engine column.
pub fn engine_label(spec: &BackupSpec) -> String {
    match &spec.engine {
        BackupEngine::Logical { .. } => "logical".into(),
        BackupEngine::Native { tool, .. } => enum_text(tool),
    }
}

/// Next run of `job` after `from` as RFC 3339, `None` when manual or disabled.
pub fn next_run_text(job: &BackupJob, from: DateTime<Utc>) -> Option<String> {
    if !job.enabled {
        return None;
    }
    let s = job.schedule.as_ref()?;
    next_after(s, from)
        .ok()
        .flatten()
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

// ---------------------------------------------------------------------------------------------
// Locks
// ---------------------------------------------------------------------------------------------

/// Lock file of a job, shared by the app and the CLI. Hold the guard of
/// [`fd_lock::RwLock::try_write`] while the job runs.
pub fn open_lock(locks_dir: &Path, job_id: Uuid) -> Result<fd_lock::RwLock<File>> {
    fs::create_dir_all(locks_dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(locks_dir.join(format!("backup-{job_id}.lock")))?;
    Ok(fd_lock::RwLock::new(file))
}

fn busy() -> Error {
    Error::Busy("this backup job is already running".into())
}

// ---------------------------------------------------------------------------------------------
// Run records
// ---------------------------------------------------------------------------------------------

/// Reporter wrapper keeping the tail of the log for the run record.
struct LogReporter {
    inner: Arc<dyn JobReporter>,
    tail: Mutex<String>,
}

impl JobReporter for LogReporter {
    fn progress(&self, p: &JobProgress) {
        self.inner.progress(p);
    }

    fn log(&self, line: &str) {
        self.inner.log(line);
        let mut t = self.tail.lock().unwrap_or_else(|e| e.into_inner());
        t.push_str(line);
        t.push('\n');
        if t.len() > LOG_TAIL_BYTES {
            let mut cut = t.len() - LOG_TAIL_BYTES;
            while !t.is_char_boundary(cut) {
                cut += 1;
            }
            t.drain(..cut);
        }
    }

    fn cancel_token(&self) -> &CancelToken {
        self.inner.cancel_token()
    }
}

/// Describes a run for the history.
#[derive(Debug, Clone)]
pub struct RunMeta {
    /// Job, when started from one.
    pub job_id: Option<Uuid>,
    /// Job name (or a label for manual runs).
    pub job_name: String,
    /// Connection.
    pub connection_id: Option<Uuid>,
    /// Backup, restore or verify.
    pub kind: RunKind,
    /// What started it.
    pub trigger: RunTrigger,
    /// `logical` or the native tool name.
    pub engine: String,
}

/// What a finished piece of work reports into its run record.
#[derive(Debug, Clone, Default)]
pub struct RunOutput {
    /// Output file.
    pub file_path: Option<String>,
    /// Output size.
    pub size_bytes: Option<u64>,
    /// Checksum.
    pub checksum: Option<String>,
    /// Objects processed.
    pub objects: Option<u32>,
    /// Rows processed.
    pub rows: Option<u64>,
}

impl From<&BackupOutcome> for RunOutput {
    fn from(o: &BackupOutcome) -> Self {
        RunOutput {
            file_path: o.files.first().cloned(),
            size_bytes: Some(o.size_bytes),
            checksum: Some(o.checksum.clone()).filter(|c| !c.is_empty()),
            objects: Some(o.objects),
            rows: Some(o.rows),
        }
    }
}

impl From<&RestoreOutcome> for RunOutput {
    fn from(o: &RestoreOutcome) -> Self {
        RunOutput {
            objects: Some(o.objects),
            rows: Some(o.rows),
            ..Default::default()
        }
    }
}

fn now_text() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Record a run around `work`: a `running` row first, the final status afterwards. The row is
/// updated even when the work fails or is cancelled. Returns the final record.
pub async fn record_run<F, Fut>(
    env: &RunEnv,
    meta: RunMeta,
    reporter: Arc<dyn JobReporter>,
    work: F,
) -> Result<BackupRun>
where
    F: FnOnce(Arc<dyn JobReporter>) -> Fut,
    Fut: Future<Output = Result<RunOutput>>,
{
    let started = Utc::now();
    let mut run = BackupRun {
        id: Uuid::new_v4(),
        job_id: meta.job_id,
        job_name: meta.job_name,
        connection_id: meta.connection_id,
        kind: meta.kind,
        trigger: meta.trigger,
        engine: meta.engine,
        status: RunStatus::Running,
        started_at: started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        finished_at: None,
        duration_ms: None,
        file_path: None,
        size_bytes: None,
        checksum: None,
        objects: None,
        rows: None,
        error: None,
        log_tail: None,
    };
    insert_run(env, &run)?;
    let logger = Arc::new(LogReporter {
        inner: reporter,
        tail: Mutex::new(String::new()),
    });
    let result = work(logger.clone()).await;
    run.finished_at = Some(now_text());
    run.duration_ms = Some(u64::try_from((Utc::now() - started).num_milliseconds()).unwrap_or(0));
    match result {
        Ok(out) => {
            run.status = RunStatus::Succeeded;
            run.file_path = out.file_path;
            run.size_bytes = out.size_bytes;
            run.checksum = out.checksum;
            run.objects = out.objects;
            run.rows = out.rows;
        }
        Err(Error::Cancelled) => run.status = RunStatus::Cancelled,
        Err(e) => {
            run.status = RunStatus::Failed;
            run.error = Some(JobError {
                kind: e.kind().into(),
                message: e.message(),
            });
            logger.log(&format!("failed: {}", e.message()));
        }
    }
    run.log_tail = Some(
        logger
            .tail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
    )
    .filter(|t| !t.is_empty());
    env.service.store().backup_run_update(&run_to_row(&run))?;
    Ok(run)
}

// ---------------------------------------------------------------------------------------------
// Connecting
// ---------------------------------------------------------------------------------------------

/// [`dbstudio_ssh::KnownHosts`] backed by the `ssh_known_hosts` table. A host that is unknown or
/// changed stops the run: nobody is there to confirm the key.
struct StoreKnownHosts(Arc<ConnectionService>);

impl dbstudio_ssh::KnownHosts for StoreKnownHosts {
    fn check(
        &self,
        host: &str,
        port: u16,
        key_type: &str,
        fingerprint: &str,
    ) -> dbstudio_ssh::HostKeyStatus {
        use dbstudio_ssh::HostKeyStatus;
        match self.0.store().known_hosts_for(host, port) {
            Ok(keys) if keys.is_empty() => HostKeyStatus::Unknown,
            Ok(keys) => {
                if keys
                    .iter()
                    .any(|k| k.key_type == key_type && k.fingerprint == fingerprint)
                {
                    HostKeyStatus::Known
                } else {
                    HostKeyStatus::Changed
                }
            }
            Err(_) => HostKeyStatus::Unknown,
        }
    }
}

/// A connection for one run. With an SSH profile the run opens its own tunnel (the same
/// bastion, secret and trusted host keys the application uses) that lives as long as this value:
/// the session connects to its local end and native tools get it as `tunnel_addr`.
pub struct Connected {
    /// Profile settings as saved (the real host and port).
    pub config: ConnectionConfig,
    /// Stored password.
    pub secret: Option<Secret>,
    /// Open session.
    pub session: Arc<dyn Session>,
    /// Local end of the SSH tunnel, when the profile has one.
    pub tunnel_addr: Option<std::net::SocketAddr>,
    _tunnel: Option<dbstudio_ssh::Tunnel>,
}

impl std::fmt::Debug for Connected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connected")
            .field("config", &self.config)
            .field("tunnel_addr", &self.tunnel_addr)
            .finish_non_exhaustive()
    }
}

/// Open a session (and the SSH tunnel) for `connection_id`.
pub async fn connect(env: &RunEnv, connection_id: Uuid) -> Result<Connected> {
    let svc = env.service.clone();
    let secrets = env.secrets.clone();
    let (profile, secret, ssh_secret) = tokio::task::spawn_blocking(move || {
        let (profile, secret) = svc.session_credentials(connection_id, None)?;
        let ssh_secret = if profile.config.ssh.is_some() {
            secrets.get_account(&ssh_secret_account(connection_id))?
        } else {
            None
        };
        Ok::<_, Error>((profile, secret, ssh_secret))
    })
    .await
    .map_err(|e| Error::Other(format!("background task failed: {e}")))??;
    let driver = env.registry.get(profile.config.kind).ok_or_else(|| {
        Error::Unsupported(format!("driver for {}", profile.config.kind.display_name()))
    })?;
    let config = profile.config;
    let (driver_config, tunnel) = match &config.ssh {
        Some(cfg) => {
            let known = StoreKnownHosts(env.service.clone());
            let t = dbstudio_ssh::Tunnel::open(
                cfg,
                ssh_secret.as_ref(),
                (config.host.clone(), config.port),
                &known,
            )
            .await?;
            let mut c = config.clone();
            c.host = "127.0.0.1".to_string();
            c.port = t.local_addr().port();
            if c.tls.server_name.is_none() {
                c.tls.server_name = Some(config.host.clone());
            }
            (c, Some(t))
        }
        None => (config.clone(), None),
    };
    let session: Arc<dyn Session> = Arc::from(
        driver
            .connect(
                &driver_config,
                driver_config.effective_secret(secret.as_ref()),
            )
            .await?,
    );
    Ok(Connected {
        tunnel_addr: tunnel.as_ref().map(dbstudio_ssh::Tunnel::local_addr),
        config,
        secret,
        session,
        _tunnel: tunnel,
    })
}

/// Native tool paths configured in settings.
pub fn tool_overrides(env: &RunEnv) -> Result<BTreeMap<NativeTool, String>> {
    Ok(env
        .service
        .store()
        .get_native_tools()?
        .into_iter()
        .filter_map(|(k, v)| {
            serde_json::from_value::<NativeTool>(serde_json::Value::String(k))
                .ok()
                .map(|t| (t, v))
        })
        .collect())
}

/// Run `spec` once and return its outcome (no run record).
pub async fn execute_backup(
    env: &RunEnv,
    spec: &BackupSpec,
    passphrase: Option<Secret>,
    job_name: Option<String>,
    reporter: Arc<dyn JobReporter>,
) -> Result<BackupOutcome> {
    let conn = connect(env, spec.connection_id).await?;
    let session = conn.session.clone();
    let ctx = BackupContext {
        session: session.clone(),
        config: conn.config.clone(),
        secret: conn.secret.clone(),
        tunnel_addr: conn.tunnel_addr,
        reporter,
        job_name,
        tool_overrides: tool_overrides(env)?,
        work_dir: Some(env.work_dir.clone()),
    };
    let out = run_backup(ctx, spec, passphrase.as_ref()).await;
    if let Err(e) = session.close().await {
        tracing::debug!(error = %e, "closing backup session failed");
    }
    out
}

/// Restore into `spec.target_connection_id` (callers check production confirmation first).
pub async fn execute_restore(
    env: &RunEnv,
    spec: &RestoreSpec,
    passphrase: Option<Secret>,
    reporter: Arc<dyn JobReporter>,
) -> Result<RestoreOutcome> {
    let conn = connect(env, spec.target_connection_id).await?;
    let session = conn.session.clone();
    if restore_is_guarded(&conn.config, spec) && !restore_names_its_target(spec) {
        let _ = session.close().await;
        return Err(explicit_target_required());
    }
    if conn.config.read_only {
        let _ = session.close().await;
        return Err(Error::ReadOnlyViolation {
            message: "restore is not allowed on a read-only connection".into(),
            position: None,
        });
    }
    let ctx = BackupContext {
        session: session.clone(),
        config: conn.config.clone(),
        secret: conn.secret.clone(),
        tunnel_addr: conn.tunnel_addr,
        reporter,
        job_name: None,
        tool_overrides: tool_overrides(env)?,
        work_dir: Some(env.work_dir.clone()),
    };
    let out = run_restore(ctx, spec, passphrase.as_ref()).await;
    if let Err(e) = session.close().await {
        tracing::debug!(error = %e, "closing restore session failed");
    }
    out
}

/// A restore that needs a confirmation: the target connection is `production`, or the restore
/// drops existing objects (`clean`).
pub fn restore_is_guarded(config: &ConnectionConfig, spec: &RestoreSpec) -> bool {
    config.environment == Some(Environment::Production) || spec.clean
}

/// A guarded restore must name the database it writes to: with the default (the database the
/// backup came from) the confirmation text could not say which database is at stake.
pub fn restore_names_its_target(spec: &RestoreSpec) -> bool {
    spec.target_database
        .as_deref()
        .is_some_and(|d| !d.is_empty())
}

/// Error for a guarded restore without an explicit target database.
pub fn explicit_target_required() -> Error {
    Error::Invalid(
        "a restore into a production connection, or one that cleans the target, needs the \
         target database to be named explicitly"
            .into(),
    )
}

/// Text the user must type to restore into `config`, when a confirmation is needed: the target
/// database named in `spec` (the database that is actually written).
pub fn restore_confirmation_text(config: &ConnectionConfig, spec: &RestoreSpec) -> Option<String> {
    if !restore_is_guarded(config, spec) {
        return None;
    }
    let db = spec.target_database.clone().filter(|d| !d.is_empty())?;
    // Redis databases are numbers: the text to type is `db<N>` so it cannot be mistaken for a
    // count.
    Some(if config.kind == dbstudio_core::DbKind::Redis {
        format!("db{db}")
    } else {
        db
    })
}

// ---------------------------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------------------------

/// Run a saved job: lock, record, back up, retention, schedule bookkeeping. The returned run has
/// the final status (a failed backup is `Ok(run)` with `status = Failed`); `Err` means the job
/// could not even start (lock held, store error).
pub async fn run_job(
    env: &RunEnv,
    job: &BackupJob,
    trigger: RunTrigger,
    reporter: Arc<dyn JobReporter>,
) -> Result<BackupRun> {
    let mut lock = open_lock(&env.locks_dir, job.id)?;
    let _guard = lock.try_write().map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            busy()
        } else {
            Error::Io(e)
        }
    })?;
    let meta = RunMeta {
        job_id: Some(job.id),
        job_name: job.name.clone(),
        connection_id: Some(job.spec.connection_id),
        kind: RunKind::Backup,
        trigger,
        engine: engine_label(&job.spec),
    };
    let spec = job.spec.clone();
    let name = job.name.clone();
    let job_id = job.id;
    let env2 = env.clone();
    let run = record_run(env, meta, reporter, move |rep| async move {
        let passphrase = if spec.encrypted {
            let svc = env2.secrets.clone();
            let account = backup_secret_account(job_id);
            let found = tokio::task::spawn_blocking(move || svc.get_account(&account))
                .await
                .map_err(|e| Error::Other(format!("background task failed: {e}")))??;
            Some(found.ok_or_else(|| {
                Error::Other(
                    "the passphrase of this job was not found in the keychain; edit the job and set it again"
                        .into(),
                )
            })?)
        } else {
            None
        };
        let out = execute_backup(&env2, &spec, passphrase, Some(name), rep).await?;
        Ok(RunOutput::from(&out))
    })
    .await?;

    let finished = Utc::now();
    // The job may have been edited (new schedule, retention) or deleted while it ran: the
    // bookkeeping uses what is stored now, and a deleted job just leaves its run record.
    let store = env.service.store();
    let current = match store.backup_job_get(job.id)? {
        Some(row) => row_to_job(&row).ok(),
        None => None,
    };
    if let Some(current) = current {
        if run.status == RunStatus::Succeeded {
            let protect: Vec<PathBuf> = run.file_path.iter().map(PathBuf::from).collect();
            apply_job_retention(env, &current, finished, &protect);
        }
        store.backup_job_set_schedule_state(
            job.id,
            None,
            Some(&run.started_at),
            next_run_text(&current, finished).as_deref(),
        )?;
    }
    Ok(run)
}

/// Record a run that did not happen (the job was already running, the lock or the store failed)
/// so that the history and the notification say why instead of the slot silently passing.
pub fn record_skipped(
    env: &RunEnv,
    job: &BackupJob,
    trigger: RunTrigger,
    why: &Error,
) -> Result<BackupRun> {
    let now = now_text();
    let run = BackupRun {
        id: Uuid::new_v4(),
        job_id: Some(job.id),
        job_name: job.name.clone(),
        connection_id: Some(job.spec.connection_id),
        kind: RunKind::Backup,
        trigger,
        engine: engine_label(&job.spec),
        status: RunStatus::Failed,
        started_at: now.clone(),
        finished_at: Some(now),
        duration_ms: Some(0),
        file_path: None,
        size_bytes: None,
        checksum: None,
        objects: None,
        rows: None,
        error: Some(JobError {
            kind: why.kind().into(),
            message: format!("not run: {}", why.message()),
        }),
        log_tail: None,
    };
    insert_run(env, &run)?;
    Ok(run)
}

/// Insert a run row; a job or connection deleted in the meantime only loses its link.
fn insert_run(env: &RunEnv, run: &BackupRun) -> Result<()> {
    let store = env.service.store();
    match store.backup_run_insert(&run_to_row(run)) {
        Ok(()) => Ok(()),
        Err(_) => {
            let mut unlinked = run.clone();
            unlinked.job_id = None;
            unlinked.connection_id = None;
            store.backup_run_insert(&run_to_row(&unlinked))
        }
    }
}

/// Age after which a scratch directory or temporary file is considered abandoned.
const ORPHAN_AGE: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// Remove what a crashed run left behind: scratch directories in `<work_dir>/tmp` and
/// half-written `.part` files in the job destinations, when older than [`ORPHAN_AGE`].
/// Returns how many entries were removed.
pub fn cleanup_orphans(env: &RunEnv) -> usize {
    let mut removed = 0;
    let old = |p: &Path| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= ORPHAN_AGE)
    };
    if let Ok(rd) = fs::read_dir(env.work_dir.join("tmp")) {
        for e in rd.flatten() {
            let p = e.path();
            if old(&p) {
                let ok = if p.is_dir() {
                    fs::remove_dir_all(&p).is_ok()
                } else {
                    fs::remove_file(&p).is_ok()
                };
                removed += usize::from(ok);
            }
        }
    }
    let dirs: std::collections::BTreeSet<PathBuf> = env
        .service
        .store()
        .backup_jobs_list()
        .unwrap_or_default()
        .iter()
        .filter_map(|r| serde_json::from_str::<BackupSpec>(&r.spec).ok())
        .filter_map(|spec| match spec.destination {
            crate::Destination::LocalDir { path, .. } => Some(PathBuf::from(path)),
            _ => None,
        })
        .collect();
    for dir in dirs {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let p = e.path();
            if name.starts_with('.') && name.ends_with(".part") && old(&p) {
                removed += usize::from(fs::remove_file(&p).is_ok());
            }
        }
    }
    removed
}

/// Delete the files retention selects (best effort; failures are logged).
pub fn apply_job_retention(
    env: &RunEnv,
    job: &BackupJob,
    now: DateTime<Utc>,
    protect: &[PathBuf],
) -> Vec<PathBuf> {
    let runs = match env.service.store().backup_runs_list(Some(job.id), 10_000) {
        Ok(rows) => rows
            .iter()
            .filter_map(|r| row_to_run(r).ok())
            .collect::<Vec<_>>(),
        Err(e) => {
            tracing::warn!(error = %e, "retention: cannot read runs");
            return Vec::new();
        }
    };
    let protect: Vec<&Path> = protect.iter().map(PathBuf::as_path).collect();
    let doomed = crate::apply_retention_protecting(job, &runs, now, &protect);
    let mut deleted = Vec::new();
    for path in doomed {
        match fs::remove_file(&path) {
            Ok(()) => deleted.push(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => deleted.push(path),
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "retention: cannot delete")
            }
        }
    }
    // Forget the deleted files so the same path is not tried again.
    for run in runs.iter().filter(|r| {
        r.file_path
            .as_ref()
            .is_some_and(|p| deleted.iter().any(|d| d == Path::new(p)))
    }) {
        let mut row = run_to_row(run);
        row.file_path = None;
        let _ = env.service.store().backup_run_update(&row);
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Compression, Destination, NotifyPolicy, Retention};

    fn job() -> BackupJob {
        BackupJob {
            id: Uuid::from_u128(5),
            name: "nightly".into(),
            enabled: true,
            spec: BackupSpec {
                connection_id: Uuid::from_u128(6),
                targets: vec![],
                engine: BackupEngine::Native {
                    tool: NativeTool::PgDump,
                    options: BTreeMap::new(),
                },
                compression: Compression::Gzip { level: 5 },
                encrypted: true,
                destination: Destination::LocalDir {
                    path: "/b".into(),
                    file_template: "x".into(),
                },
            },
            schedule: Some(Schedule {
                cron: "0 2 * * *".into(),
                timezone: ScheduleTz::Utc,
            }),
            retention: Retention {
                keep_last: Some(3),
                max_age_days: Some(30),
            },
            missed_run: MissedRunPolicy::Skip,
            notify: NotifyPolicy {
                on_success: false,
                on_failure: true,
            },
            last_run_at: Some("2026-01-01T00:00:00Z".into()),
            next_run_at: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn job_row_roundtrip() {
        let j = job();
        let row = job_to_row(&j, Some("s".into())).unwrap();
        assert_eq!(row.cron.as_deref(), Some("0 2 * * *"));
        assert_eq!(row.timezone, "utc");
        assert_eq!(row.missed_policy, "skip");
        assert!(row.encrypted);
        assert_eq!(row_to_job(&row).unwrap(), j);
        let mut manual = j.clone();
        manual.schedule = None;
        assert_eq!(
            row_to_job(&job_to_row(&manual, None).unwrap()).unwrap(),
            manual
        );
    }

    #[test]
    fn run_row_roundtrip_and_labels() {
        let run = BackupRun {
            id: Uuid::from_u128(1),
            job_id: Some(Uuid::from_u128(5)),
            job_name: "n".into(),
            connection_id: None,
            kind: RunKind::Restore,
            trigger: RunTrigger::CatchUp,
            engine: "logical".into(),
            status: RunStatus::Interrupted,
            started_at: "2026-01-01T00:00:00Z".into(),
            finished_at: None,
            duration_ms: Some(3),
            file_path: Some("/f".into()),
            size_bytes: Some(9),
            checksum: Some("ab".into()),
            objects: Some(2),
            rows: Some(7),
            error: Some(JobError {
                kind: "io".into(),
                message: "disk".into(),
            }),
            log_tail: Some("x".into()),
        };
        let row = run_to_row(&run);
        assert_eq!(
            (row.kind.as_str(), row.trigger.as_str(), row.status.as_str()),
            ("restore", "catchUp", "interrupted")
        );
        assert_eq!(row_to_run(&row).unwrap(), run);
        assert_eq!(engine_label(&job().spec), "pgDump");
    }

    #[test]
    fn restore_confirmation_rules() {
        let mut c = ConnectionConfig::new(dbstudio_core::DbKind::Postgres, "prod-pg", "h");
        c.database = Some("main".into());
        let mut spec = RestoreSpec {
            file: "f".into(),
            target_connection_id: Uuid::nil(),
            target_database: None,
            create_database: false,
            clean: false,
            objects: vec![],
            continue_on_error: false,
        };
        assert_eq!(restore_confirmation_text(&c, &spec), None);
        // Production: the text is the database that is written, which must be named.
        c.environment = Some(Environment::Production);
        assert!(restore_is_guarded(&c, &spec));
        assert!(!restore_names_its_target(&spec));
        assert_eq!(restore_confirmation_text(&c, &spec), None);
        spec.target_database = Some("copy".into());
        assert!(restore_names_its_target(&spec));
        assert_eq!(
            restore_confirmation_text(&c, &spec).as_deref(),
            Some("copy")
        );
        spec.target_database = Some(String::new());
        assert!(!restore_names_its_target(&spec));
        c.environment = Some(Environment::Development);
        spec.target_database = Some("copy".into());
        assert_eq!(restore_confirmation_text(&c, &spec), None);
        spec.clean = true;
        assert!(restore_is_guarded(&c, &spec));
        assert_eq!(
            restore_confirmation_text(&c, &spec).as_deref(),
            Some("copy")
        );
    }

    fn env_in(dir: &Path) -> RunEnv {
        let secrets = Arc::new(dbstudio_store::MemorySecretStore::new());
        RunEnv {
            service: Arc::new(ConnectionService::new(
                dbstudio_store::Store::open_in_memory().unwrap(),
                secrets.clone(),
            )),
            secrets,
            registry: Arc::new(DriverRegistry::new()),
            locks_dir: dir.join("locks"),
            work_dir: dir.to_path_buf(),
        }
    }

    #[test]
    fn a_slot_that_did_not_run_leaves_a_failed_record_that_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path());
        let j = job();
        // The job and its connection are not in the store: the record is kept without links.
        let run = record_skipped(&env, &j, RunTrigger::Schedule, &busy()).unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.error.as_ref().unwrap().kind, "busy");
        assert!(run.error.as_ref().unwrap().message.starts_with("not run:"));
        let rows = env.service.store().backup_runs_list(None, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "failed");
        assert_eq!(rows[0].trigger, "schedule");
        assert_eq!(rows[0].job_id, None);
    }

    #[test]
    fn leftovers_of_dead_runs_are_swept_but_fresh_ones_are_not() {
        use std::time::{Duration, SystemTime};
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path());
        let tmp = dir.path().join("tmp");
        fs::create_dir_all(tmp.join("run-old")).unwrap();
        fs::write(tmp.join("run-old/dump.sql"), "x").unwrap();
        fs::create_dir_all(tmp.join("run-fresh")).unwrap();
        let old = SystemTime::now() - Duration::from_secs(48 * 3600);
        // Setting a directory's times needs a handle to the directory itself: on Windows that
        // takes FILE_FLAG_BACKUP_SEMANTICS and FILE_WRITE_ATTRIBUTES (`File::open` is refused).
        #[cfg(windows)]
        let handle = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .access_mode(0x0100) // FILE_WRITE_ATTRIBUTES
                .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
                .open(tmp.join("run-old"))
                .unwrap()
        };
        #[cfg(not(windows))]
        let handle = fs::File::open(tmp.join("run-old")).unwrap();
        handle.set_modified(old).unwrap();
        drop(handle);
        assert_eq!(cleanup_orphans(&env), 1);
        assert!(!tmp.join("run-old").exists());
        assert!(tmp.join("run-fresh").exists());
    }

    #[test]
    fn lock_blocks_a_second_holder() {
        let dir = tempfile::tempdir().unwrap();
        let id = Uuid::from_u128(1);
        let mut a = open_lock(dir.path(), id).unwrap();
        let mut b = open_lock(dir.path(), id).unwrap();
        let g = a.try_write().unwrap();
        let e = b.try_write().err().unwrap();
        assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock);
        drop(g);
        assert!(b.try_write().is_ok());
    }
}
