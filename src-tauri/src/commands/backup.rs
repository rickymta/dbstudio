//! Backup, restore and scheduling commands (package H). Engine and run bookkeeping live in
//! `dbstudio-backup`; this layer checks path grants, the production / read-only guard and the
//! keychain, and runs the work as background jobs.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use dbstudio_backup::chrono::{self, Utc};
use dbstudio_backup::runner::{
    RunEnv, RunMeta, RunOutput, engine_label, execute_backup, execute_restore, job_to_row,
    next_run_text, record_run, restore_confirmation_text, row_to_job, row_to_run, run_job,
};
use dbstudio_backup::{
    BackupEngine, BackupInspection, BackupJob, BackupRun, BackupSpec, Destination, NativeTool,
    NativeToolInfo, RestoreSpec, RunKind, RunStatus, RunTrigger,
};
use dbstudio_core::{
    ConfirmationGrant, ConfirmationRequest, Error, JobId, JobKind, JobReporter, Secret,
};
use dbstudio_store::{SecretChange, backup_secret_account};
use tauri::State;
use uuid::Uuid;

use super::blocking;
use crate::events::{BACKUP_CHANGED, EventEmitter};
use crate::grants::Purpose;
use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

/// Keychain account of the passphrase saved by a manual (job-less) backup.
fn manual_secret_account(connection_id: Uuid) -> String {
    format!("backup:manual:{connection_id}")
}

fn tool_key(tool: NativeTool) -> String {
    serde_json::to_value(tool)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Configured tool paths from the settings store, keyed by [`NativeTool`].
fn tool_overrides(state: &AppState) -> Result<BTreeMap<NativeTool, String>> {
    let stored = state.connections.store().get_native_tools()?;
    Ok(stored
        .into_iter()
        .filter_map(|(k, v)| {
            serde_json::from_value::<NativeTool>(serde_json::Value::String(k))
                .ok()
                .map(|t| (t, v))
        })
        .collect())
}

/// Run environment of the engine.
pub(crate) fn run_env(state: &AppState) -> RunEnv {
    RunEnv {
        service: state.connections.clone(),
        secrets: state.secrets.clone(),
        registry: state.registry.clone(),
        locks_dir: crate::background::locks_dir(),
    }
}

fn emit_changed(state: &AppState) {
    state.events.emit(BACKUP_CHANGED, serde_json::Value::Null);
}

fn now_text() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A local destination directory must have been picked in a dialog, unless the job already
/// stores exactly that directory.
fn check_destination(
    state: &AppState,
    spec: &BackupSpec,
    stored: Option<&BackupSpec>,
) -> Result<()> {
    let Destination::LocalDir { path, .. } = &spec.destination else {
        return Ok(());
    };
    if let Some(Destination::LocalDir { path: old, .. }) = stored.map(|s| &s.destination) {
        if old == path {
            return Ok(());
        }
    }
    state.grants.check(Path::new(path), Purpose::Dir)?;
    Ok(())
}

fn validate_spec(state: &AppState, spec: &BackupSpec) -> Result<()> {
    let profile = state.connections.get_profile(spec.connection_id)?;
    if let BackupEngine::Native { tool, options } = &spec.engine {
        dbstudio_backup::native::validate_options(*tool, options)?;
    }
    match (&spec.destination, &spec.engine) {
        (Destination::ServerDir { .. }, BackupEngine::Native { .. }) => {}
        (Destination::ServerDir { .. }, _) => {
            return Err(Error::Other(
                "a server directory needs the SQL Server native backup".into(),
            ));
        }
        (Destination::LocalDir { path, .. }, _) if path.trim().is_empty() => {
            return Err(Error::Other("choose a destination directory".into()));
        }
        _ => {}
    }
    let _ = profile;
    Ok(())
}

pub(crate) async fn jobs_list_impl(state: &AppState) -> Result<Vec<BackupJob>> {
    let svc = state.connections.clone();
    blocking(move || {
        svc.store()
            .backup_jobs_list()?
            .iter()
            .map(row_to_job)
            .collect()
    })
    .await
}

pub(crate) async fn job_save_impl(
    state: &AppState,
    mut job: BackupJob,
    passphrase: SecretChange,
) -> Result<BackupJob> {
    if job.name.trim().is_empty() {
        return Err(Error::Other("the job needs a name".into()));
    }
    if let Some(s) = &job.schedule {
        dbstudio_backup::parse_cron(&s.cron)?;
    }
    let svc = state.connections.clone();
    let existing = {
        let svc = svc.clone();
        let id = job.id;
        blocking(move || svc.store().backup_job_get(id)).await?
    };
    let stored_spec = existing
        .as_ref()
        .map(row_to_job)
        .transpose()?
        .map(|j| j.spec);
    validate_spec(state, &job.spec)?;
    check_destination(state, &job.spec, stored_spec.as_ref())?;

    let secrets = state.secrets.clone();
    let account = backup_secret_account(job.id);
    let encrypted = job.spec.encrypted;
    blocking(move || {
        match (encrypted, passphrase) {
            (true, SecretChange::Set { value }) => {
                if value.expose().is_empty() {
                    return Err(Error::Other("the passphrase cannot be empty".into()));
                }
                secrets.set_account(&account, &value)?;
            }
            (true, SecretChange::Keep) => {
                if secrets.get_account(&account)?.is_none() {
                    return Err(Error::Other("an encrypted job needs a passphrase".into()));
                }
            }
            (true, SecretChange::Clear) => {
                return Err(Error::Other("an encrypted job needs a passphrase".into()));
            }
            (false, _) => secrets.delete_account(&account)?,
        }
        Ok(())
    })
    .await?;

    let now = Utc::now();
    job.updated_at = now_text();
    job.created_at = existing
        .as_ref()
        .map_or_else(now_text, |r| r.created_at.clone());
    job.last_run_at = existing.as_ref().and_then(|r| r.last_run_at.clone());
    // A new or re-scheduled job starts counting slots from now.
    let new_cron = job.schedule.as_ref().map(|s| s.cron.clone());
    let new_tz = job
        .schedule
        .as_ref()
        .and_then(|s| serde_json::to_value(s.timezone).ok())
        .and_then(|v| v.as_str().map(str::to_string));
    let schedule_changed = existing.as_ref().is_none_or(|r| {
        r.cron != new_cron || (new_cron.is_some() && Some(r.timezone.clone()) != new_tz)
    });
    let last_scheduled = if schedule_changed {
        Some(now_text())
    } else {
        existing.and_then(|r| r.last_scheduled_for)
    };
    job.next_run_at = next_run_text(&job, now);
    let row = job_to_row(&job, last_scheduled)?;
    let saved = blocking(move || svc.store().backup_job_save(&row)).await?;
    emit_changed(state);
    row_to_job(&saved)
}

pub(crate) async fn job_delete_impl(state: &AppState, id: Uuid) -> Result<()> {
    let svc = state.connections.clone();
    let secrets = state.secrets.clone();
    blocking(move || {
        svc.store().backup_job_delete(id)?;
        // A missing keychain entry is fine; a locked keychain must not keep the job alive.
        if let Err(e) = secrets.delete_account(&backup_secret_account(id)) {
            tracing::warn!(error = %e, "cannot remove the job passphrase from the keychain");
        }
        Ok(())
    })
    .await?;
    emit_changed(state);
    Ok(())
}

/// Start `job` as a background job (shared by "run now" and the scheduler): lock, record,
/// retention, notification and the `backup://changed` event.
pub(crate) fn spawn_job_run(state: &AppState, job: BackupJob, trigger: RunTrigger) -> JobId {
    let env = run_env(state);
    let host = state.host.clone();
    let events = state.events.clone();
    let title = format!("Backup {}", job.name);
    state.jobs.spawn(
        JobKind::Backup,
        title,
        Some(job.spec.connection_id),
        move |ctx| async move {
            let reporter: Arc<dyn JobReporter> = ctx;
            let result = run_job(&env, &job, trigger, reporter).await;
            if let Ok(run) = &result {
                crate::background::notify_run(host.as_ref(), &job, run);
            }
            events.emit(BACKUP_CHANGED, serde_json::Value::Null);
            let run = result?;
            match run.status {
                RunStatus::Succeeded => Ok(Some(serde_json::json!({
                    "path": run.file_path,
                    "sizeBytes": run.size_bytes,
                    "rows": run.rows,
                }))),
                RunStatus::Cancelled => Err(Error::Cancelled),
                _ => Err(Error::Other(
                    run.error
                        .map_or_else(|| "backup failed".into(), |e| e.message),
                )),
            }
        },
    )
}

pub(crate) async fn job_run_now_impl(state: &AppState, id: Uuid) -> Result<JobId> {
    let svc = state.connections.clone();
    let row = blocking(move || svc.store().backup_job_get(id))
        .await?
        .ok_or_else(|| Error::NotFound(format!("backup job {id}")))?;
    let job = row_to_job(&row)?;
    Ok(spawn_job_run(state, job, RunTrigger::Manual))
}

pub(crate) async fn run_manual_impl(
    state: &AppState,
    spec: BackupSpec,
    passphrase: Option<Secret>,
    save_passphrase: bool,
) -> Result<JobId> {
    validate_spec(state, &spec)?;
    check_destination(state, &spec, None)?;
    let account = manual_secret_account(spec.connection_id);
    let secrets = state.secrets.clone();
    let encrypted = spec.encrypted;
    let (acc2, pass2) = (account.clone(), passphrase.clone());
    let passphrase = blocking(move || {
        if !encrypted {
            return Ok(None);
        }
        match pass2 {
            Some(p) if p.expose().is_empty() => {
                Err(Error::Other("the passphrase cannot be empty".into()))
            }
            Some(p) => {
                if save_passphrase {
                    secrets.set_account(&acc2, &p)?;
                }
                Ok(Some(p))
            }
            // Reuse the passphrase saved by an earlier manual backup of this connection.
            None => secrets
                .get_account(&acc2)?
                .map(Some)
                .ok_or_else(|| Error::Other("an encrypted backup needs a passphrase".into())),
        }
    })
    .await?;
    let env = run_env(state);
    let events = state.events.clone();
    let meta = RunMeta {
        job_id: None,
        job_name: "manual".into(),
        connection_id: Some(spec.connection_id),
        kind: RunKind::Backup,
        trigger: RunTrigger::Manual,
        engine: engine_label(&spec),
    };
    Ok(state.jobs.spawn(
        JobKind::Backup,
        "Backup",
        Some(spec.connection_id),
        move |ctx| async move {
            let reporter: Arc<dyn JobReporter> = ctx;
            let env2 = env.clone();
            let run = record_run(&env, meta, reporter, move |rep| async move {
                let out = execute_backup(&env2, &spec, passphrase, None, rep).await?;
                Ok(RunOutput::from(&out))
            })
            .await;
            events.emit(BACKUP_CHANGED, serde_json::Value::Null);
            finish(run?)
        },
    ))
}

/// Job result of a finished run record.
fn finish(run: BackupRun) -> Result<Option<serde_json::Value>> {
    match run.status {
        RunStatus::Succeeded => Ok(Some(serde_json::json!({
            "path": run.file_path,
            "sizeBytes": run.size_bytes,
            "rows": run.rows,
        }))),
        RunStatus::Cancelled => Err(Error::Cancelled),
        _ => Err(Error::Other(
            run.error.map_or_else(|| "failed".into(), |e| e.message),
        )),
    }
}

pub(crate) async fn runs_list_impl(
    state: &AppState,
    job_id: Option<Uuid>,
    limit: u32,
    before: Option<String>,
) -> Result<Vec<BackupRun>> {
    let svc = state.connections.clone();
    blocking(move || {
        svc.store()
            .backup_runs_page(job_id, limit.clamp(1, 500), before.as_deref())?
            .iter()
            .map(row_to_run)
            .collect()
    })
    .await
}

pub(crate) async fn inspect_impl(
    state: &AppState,
    path: String,
    passphrase: Option<Secret>,
) -> Result<BackupInspection> {
    let granted = state.grants.check(Path::new(&path), Purpose::Read)?;
    dbstudio_backup::inspect(&granted, passphrase.as_ref()).await
}

pub(crate) async fn verify_impl(
    state: &AppState,
    path: String,
    passphrase: Option<Secret>,
) -> Result<JobId> {
    let granted = state.grants.check(Path::new(&path), Purpose::Read)?;
    let env = run_env(state);
    let events = state.events.clone();
    let meta = RunMeta {
        job_id: None,
        job_name: granted
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        connection_id: None,
        kind: RunKind::Verify,
        trigger: RunTrigger::Manual,
        engine: "verify".into(),
    };
    Ok(state.jobs.spawn(
        JobKind::Verify,
        "Verify backup",
        None,
        move |ctx| async move {
            let reporter: Arc<dyn JobReporter> = ctx;
            let run = record_run(&env, meta, reporter, move |rep| async move {
                dbstudio_backup::verify(&granted, passphrase.as_ref(), rep.as_ref()).await?;
                Ok(RunOutput {
                    file_path: Some(granted.to_string_lossy().into_owned()),
                    size_bytes: std::fs::metadata(&granted).map(|m| m.len()).ok(),
                    ..Default::default()
                })
            })
            .await;
            events.emit(BACKUP_CHANGED, serde_json::Value::Null);
            finish(run?)
        },
    ))
}

/// Everything that identifies what a restore confirmation was issued for.
fn restore_payload(spec: &RestoreSpec) -> Vec<u8> {
    serde_json::to_vec(&(
        &spec.file,
        spec.target_connection_id,
        &spec.target_database,
        spec.create_database,
        spec.clean,
        &spec.objects,
        spec.continue_on_error,
    ))
    .unwrap_or_default()
}

pub(crate) async fn restore_start_impl(
    state: &AppState,
    mut spec: RestoreSpec,
    passphrase: Option<Secret>,
    confirm: Option<ConfirmationGrant>,
) -> Result<JobId> {
    let granted = state.grants.check(Path::new(&spec.file), Purpose::Read)?;
    spec.file = granted.to_string_lossy().into_owned();
    let profile = state.connections.get_profile(spec.target_connection_id)?;
    // The script comes from a file: a read-only connection never receives it, and a production
    // connection (or a `clean` restore) needs the typed confirmation of the guard.
    crate::guard::ensure_writable(&profile.config, "restore")?;
    if let Some(text) = restore_confirmation_text(&profile.config, &spec) {
        let target = format!("restore:{}", spec.target_connection_id);
        let payload = restore_payload(&spec);
        let accepted = confirm
            .as_ref()
            .is_some_and(|g| state.confirm.consume(g, &target, &payload).is_ok());
        if !accepted {
            let (token, expires_in_ms) = state.confirm.issue(&target, &payload, &text);
            return Err(Error::ConfirmationRequired(Box::new(ConfirmationRequest {
                token,
                confirm_text: text,
                statements: Vec::new(),
                expires_in_ms,
            })));
        }
    }
    let env = run_env(state);
    let events = state.events.clone();
    let meta = RunMeta {
        job_id: None,
        job_name: granted
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        connection_id: Some(spec.target_connection_id),
        kind: RunKind::Restore,
        trigger: RunTrigger::Manual,
        engine: "restore".into(),
    };
    Ok(state.jobs.spawn(
        JobKind::Restore,
        "Restore backup",
        Some(spec.target_connection_id),
        move |ctx| async move {
            let reporter: Arc<dyn JobReporter> = ctx;
            let env2 = env.clone();
            let run = record_run(&env, meta, reporter, move |rep| async move {
                let o = execute_restore(&env2, &spec, passphrase, rep.clone()).await?;
                for e in &o.errors {
                    rep.log(&format!("statement {}: {}", e.line, e.message));
                }
                Ok(RunOutput::from(&o))
            })
            .await;
            events.emit(BACKUP_CHANGED, serde_json::Value::Null);
            finish(run?)
        },
    ))
}

pub(crate) async fn tools_detect_impl(state: &AppState) -> Result<Vec<NativeToolInfo>> {
    let overrides = tool_overrides(state)?;
    blocking(move || Ok(dbstudio_backup::detect_tools(&overrides))).await
}

/// Set (or clear) the configured path of a tool; the path must come from a file dialog.
pub(crate) async fn tool_set_path_impl(
    state: &AppState,
    tool: NativeTool,
    path: Option<String>,
) -> Result<NativeToolInfo> {
    let mut stored = state.connections.store().get_native_tools()?;
    let key = tool_key(tool);
    match path {
        Some(p) => {
            let granted = state.grants.check(Path::new(&p), Purpose::Read)?;
            stored.insert(key, granted.to_string_lossy().into_owned());
        }
        None => {
            stored.remove(&key);
        }
    }
    state.connections.store().set_native_tools(&stored)?;
    let overrides = tool_overrides(state)?;
    dbstudio_backup::detect_tools(&overrides)
        .into_iter()
        .find(|i| i.tool == tool)
        .ok_or_else(|| Error::Unsupported(format!("native tool {}", tool_key(tool))))
}

pub(crate) fn schedule_preview_impl(
    cron: String,
    timezone: String,
    count: u32,
) -> Result<Vec<String>> {
    let tz = dbstudio_backup::parse_tz(&timezone)?;
    dbstudio_backup::preview_from(&cron, tz, count, Utc::now())
}

#[tauri::command]
pub async fn backup_jobs_list(state: State<'_, AppState>) -> Result<Vec<BackupJob>> {
    tracing::debug!(command = "backup_jobs_list", "ipc call");
    jobs_list_impl(&state).await
}

#[tauri::command]
pub async fn backup_job_save(
    state: State<'_, AppState>,
    job: BackupJob,
    passphrase: SecretChange,
) -> Result<BackupJob> {
    tracing::debug!(command = "backup_job_save", job = %job.id, "ipc call");
    job_save_impl(&state, job, passphrase).await
}

#[tauri::command]
pub async fn backup_job_delete(state: State<'_, AppState>, id: Uuid) -> Result<()> {
    tracing::debug!(command = "backup_job_delete", job = %id, "ipc call");
    job_delete_impl(&state, id).await
}

#[tauri::command]
pub async fn backup_job_run_now(state: State<'_, AppState>, id: Uuid) -> Result<JobId> {
    tracing::debug!(command = "backup_job_run_now", job = %id, "ipc call");
    job_run_now_impl(&state, id).await
}

#[tauri::command]
pub async fn backup_run_manual(
    state: State<'_, AppState>,
    spec: BackupSpec,
    passphrase: Option<Secret>,
    save_passphrase: Option<bool>,
) -> Result<JobId> {
    tracing::debug!(command = "backup_run_manual", "ipc call");
    run_manual_impl(&state, spec, passphrase, save_passphrase.unwrap_or(false)).await
}

#[tauri::command]
pub async fn backup_runs_list(
    state: State<'_, AppState>,
    job_id: Option<Uuid>,
    limit: u32,
    before: Option<String>,
) -> Result<Vec<BackupRun>> {
    tracing::debug!(command = "backup_runs_list", "ipc call");
    runs_list_impl(&state, job_id, limit, before).await
}

#[tauri::command]
pub async fn backup_inspect(
    state: State<'_, AppState>,
    path: String,
    passphrase: Option<Secret>,
) -> Result<BackupInspection> {
    tracing::debug!(command = "backup_inspect", "ipc call");
    inspect_impl(&state, path, passphrase).await
}

#[tauri::command]
pub async fn backup_verify(
    state: State<'_, AppState>,
    path: String,
    passphrase: Option<Secret>,
) -> Result<JobId> {
    tracing::debug!(command = "backup_verify", "ipc call");
    verify_impl(&state, path, passphrase).await
}

#[tauri::command]
pub async fn restore_start(
    state: State<'_, AppState>,
    spec: RestoreSpec,
    passphrase: Option<Secret>,
    confirm: Option<ConfirmationGrant>,
) -> Result<JobId> {
    tracing::debug!(command = "restore_start", "ipc call");
    restore_start_impl(&state, spec, passphrase, confirm).await
}

#[tauri::command]
pub async fn backup_tools_detect(state: State<'_, AppState>) -> Result<Vec<NativeToolInfo>> {
    tracing::debug!(command = "backup_tools_detect", "ipc call");
    tools_detect_impl(&state).await
}

#[tauri::command]
pub async fn backup_tool_set_path(
    state: State<'_, AppState>,
    tool: NativeTool,
    path: Option<String>,
) -> Result<NativeToolInfo> {
    tracing::debug!(command = "backup_tool_set_path", "ipc call");
    tool_set_path_impl(&state, tool, path).await
}

#[tauri::command]
pub fn schedule_preview(cron: String, timezone: String, count: u32) -> Result<Vec<String>> {
    tracing::debug!(command = "schedule_preview", "ipc call");
    schedule_preview_impl(cron, timezone, count)
}

#[cfg(test)]
mod tests {
    use dbstudio_backup::{
        Compression, MissedRunPolicy, NotifyPolicy, Retention, Schedule, ScheduleTz,
    };
    use dbstudio_core::{ConnectionConfig, DbKind, DriverRegistry, Environment};
    use dbstudio_export::ExportContent;
    use dbstudio_store::ConnectionProfile;

    use super::*;
    use crate::state::testing::test_state;

    fn state_with_connection(env: Option<Environment>, read_only: bool) -> (AppState, Uuid) {
        let (state, _) = test_state(DriverRegistry::new());
        let mut config = ConnectionConfig::new(DbKind::Postgres, "pg", "h");
        config.database = Some("app".into());
        config.environment = env;
        config.read_only = read_only;
        let id = config.id;
        state
            .connections
            .save(
                ConnectionProfile {
                    config,
                    group_id: None,
                    color: None,
                    save_password: false,
                    has_password: false,
                    sort_order: 0,
                    created_at: String::new(),
                    updated_at: String::new(),
                },
                SecretChange::Clear,
            )
            .unwrap();
        (state, id)
    }

    fn job(conn: Uuid, dir: &Path, encrypted: bool) -> BackupJob {
        BackupJob {
            id: Uuid::new_v4(),
            name: "nightly".into(),
            enabled: true,
            spec: BackupSpec {
                connection_id: conn,
                targets: vec![],
                engine: BackupEngine::Logical {
                    content: ExportContent::Both,
                    ddl: Default::default(),
                },
                compression: Compression::None,
                encrypted,
                destination: Destination::LocalDir {
                    path: dir.to_string_lossy().into_owned(),
                    file_template: "x".into(),
                },
            },
            schedule: Some(Schedule {
                cron: "0 2 * * *".into(),
                timezone: ScheduleTz::Utc,
            }),
            retention: Retention::default(),
            missed_run: MissedRunPolicy::RunOnce,
            notify: NotifyPolicy::default(),
            last_run_at: None,
            next_run_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn set(v: &str) -> SecretChange {
        SecretChange::Set {
            value: Secret::new(v),
        }
    }

    #[test]
    fn schedule_preview_validates_and_lists() {
        let p = schedule_preview_impl("*/5 * * * *".into(), "utc".into(), 4).unwrap();
        assert_eq!(p.len(), 4);
        assert!(p[0] < p[1] && p[0].ends_with('Z'));
        assert_eq!(
            schedule_preview_impl("0 2 * * *".into(), "local".into(), 2)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            schedule_preview_impl("nope".into(), "utc".into(), 3)
                .unwrap_err()
                .kind(),
            "other"
        );
        assert!(schedule_preview_impl("* * * * *".into(), "mars".into(), 3).is_err());
    }

    #[tokio::test]
    async fn job_save_needs_a_granted_directory_and_stores_the_passphrase() {
        let (state, conn) = state_with_connection(None, false);
        let dir = tempfile::tempdir().unwrap();
        let j = job(conn, dir.path(), true);
        // The directory was not picked in a dialog.
        assert_eq!(
            job_save_impl(&state, j.clone(), set("pw"))
                .await
                .unwrap_err()
                .kind(),
            "permission"
        );
        state.grants.grant(dir.path(), Purpose::Dir).unwrap();
        // Encrypted without a passphrase is refused; with one it is kept in the keychain.
        assert!(
            job_save_impl(&state, j.clone(), SecretChange::Keep)
                .await
                .is_err()
        );
        let saved = job_save_impl(&state, j.clone(), set("pw")).await.unwrap();
        assert!(saved.next_run_at.is_some());
        assert!(!saved.created_at.is_empty());
        assert_eq!(
            state
                .secrets
                .get_account(&backup_secret_account(j.id))
                .unwrap()
                .unwrap()
                .expose(),
            "pw"
        );
        // Editing keeps the passphrase; the stored directory needs no new grant.
        let mut edited = saved.clone();
        edited.name = "renamed".into();
        let again = job_save_impl(&state, edited, SecretChange::Keep)
            .await
            .unwrap();
        assert_eq!(again.name, "renamed");
        assert_eq!(again.created_at, saved.created_at);
        // Turning encryption off removes the secret.
        let mut plain = again.clone();
        plain.spec.encrypted = false;
        job_save_impl(&state, plain, SecretChange::Clear)
            .await
            .unwrap();
        assert!(
            state
                .secrets
                .get_account(&backup_secret_account(j.id))
                .unwrap()
                .is_none()
        );
        assert_eq!(jobs_list_impl(&state).await.unwrap().len(), 1);
        // Validation.
        let mut bad = j.clone();
        bad.schedule = Some(Schedule {
            cron: "61 * * * *".into(),
            timezone: ScheduleTz::Utc,
        });
        assert!(job_save_impl(&state, bad, set("pw")).await.is_err());
        let mut unnamed = j.clone();
        unnamed.name = " ".into();
        assert!(job_save_impl(&state, unnamed, set("pw")).await.is_err());
        let mut nowhere = job(Uuid::new_v4(), dir.path(), false);
        nowhere.name = "x".into();
        assert_eq!(
            job_save_impl(&state, nowhere, SecretChange::Clear)
                .await
                .unwrap_err()
                .kind(),
            "notFound"
        );
        // Delete removes the row and the secret.
        job_delete_impl(&state, j.id).await.unwrap();
        assert!(jobs_list_impl(&state).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn manual_runs_and_listing_respect_grants_and_limits() {
        let (state, conn) = state_with_connection(None, false);
        let dir = tempfile::tempdir().unwrap();
        let j = job(conn, dir.path(), true);
        assert_eq!(
            run_manual_impl(&state, j.spec.clone(), Some(Secret::new("x")), false)
                .await
                .unwrap_err()
                .kind(),
            "permission"
        );
        state.grants.grant(dir.path(), Purpose::Dir).unwrap();
        // Encrypted manual backup without any passphrase.
        assert!(
            run_manual_impl(&state, j.spec.clone(), None, false)
                .await
                .is_err()
        );
        let id = run_manual_impl(&state, j.spec.clone(), Some(Secret::new("x")), true)
            .await
            .unwrap();
        assert!(state.jobs.get(id).is_ok());
        // The passphrase saved for the connection is reused.
        assert!(
            run_manual_impl(&state, j.spec.clone(), None, false)
                .await
                .is_ok()
        );
        assert!(runs_list_impl(&state, None, 0, None).await.unwrap().len() <= 1);
        assert_eq!(run_job_missing(&state).await, "notFound");
    }

    async fn run_job_missing(state: &AppState) -> &'static str {
        job_run_now_impl(state, Uuid::new_v4())
            .await
            .unwrap_err()
            .kind()
    }

    fn restore_spec(file: &Path, conn: Uuid) -> RestoreSpec {
        RestoreSpec {
            file: file.to_string_lossy().into_owned(),
            target_connection_id: conn,
            target_database: Some("copy".into()),
            create_database: false,
            clean: false,
            objects: vec![],
            continue_on_error: false,
        }
    }

    #[tokio::test]
    async fn restore_is_guarded_by_grants_read_only_and_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.dbsbak");
        std::fs::write(&file, b"x").unwrap();

        // Production target.
        let (state, conn) = state_with_connection(Some(Environment::Production), false);
        let spec = restore_spec(&file, conn);
        assert_eq!(
            restore_start_impl(&state, spec.clone(), None, None)
                .await
                .unwrap_err()
                .kind(),
            "permission"
        );
        state.grants.grant(&file, Purpose::Read).unwrap();
        let req = match restore_start_impl(&state, spec.clone(), None, None)
            .await
            .unwrap_err()
        {
            Error::ConfirmationRequired(r) => r,
            e => panic!("{e:?}"),
        };
        assert_eq!(req.confirm_text, "copy");
        // Wrong text and a changed request are both refused (and the token is spent).
        let wrong = ConfirmationGrant {
            token: req.token.clone(),
            typed: "nope".into(),
        };
        assert_eq!(
            restore_start_impl(&state, spec.clone(), None, Some(wrong))
                .await
                .unwrap_err()
                .kind(),
            "confirmationRequired"
        );
        let req = match restore_start_impl(&state, spec.clone(), None, None)
            .await
            .unwrap_err()
        {
            Error::ConfirmationRequired(r) => r,
            e => panic!("{e:?}"),
        };
        let mut other = spec.clone();
        other.clean = true;
        let grant = ConfirmationGrant {
            token: req.token.clone(),
            typed: "copy".into(),
        };
        assert_eq!(
            restore_start_impl(&state, other, None, Some(grant.clone()))
                .await
                .unwrap_err()
                .kind(),
            "confirmationRequired"
        );
        // The right text for the same request starts the job (which then fails: no driver).
        let req = match restore_start_impl(&state, spec.clone(), None, None)
            .await
            .unwrap_err()
        {
            Error::ConfirmationRequired(r) => r,
            e => panic!("{e:?}"),
        };
        let ok = ConfirmationGrant {
            token: req.token,
            typed: "copy".into(),
        };
        let id = restore_start_impl(&state, spec.clone(), None, Some(ok.clone()))
            .await
            .unwrap();
        assert!(state.jobs.get(id).is_ok());
        // A token cannot be used twice.
        assert_eq!(
            restore_start_impl(&state, spec, None, Some(ok))
                .await
                .unwrap_err()
                .kind(),
            "confirmationRequired"
        );

        // Read-only target: refused before anything else.
        let (state, conn) = state_with_connection(Some(Environment::Development), true);
        state.grants.grant(&file, Purpose::Read).unwrap();
        assert_eq!(
            restore_start_impl(&state, restore_spec(&file, conn), None, None)
                .await
                .unwrap_err()
                .kind(),
            "readOnly"
        );
        // Development target: no confirmation unless `clean`.
        let (state, conn) = state_with_connection(Some(Environment::Development), false);
        state.grants.grant(&file, Purpose::Read).unwrap();
        assert!(
            restore_start_impl(&state, restore_spec(&file, conn), None, None)
                .await
                .is_ok()
        );
        let mut clean = restore_spec(&file, conn);
        clean.clean = true;
        assert_eq!(
            restore_start_impl(&state, clean, None, None)
                .await
                .unwrap_err()
                .kind(),
            "confirmationRequired"
        );
    }
}
