//! Backup, restore and scheduling commands (package H). The engine is a scaffold, so most
//! commands answer `Unsupported`; the typed `BackupJob` <-> `BackupJobRow` mapping is part of
//! package H.

use std::collections::BTreeMap;
use std::path::Path;

use dbstudio_backup::{
    BackupInspection, BackupJob, BackupRun, BackupSpec, NativeTool, NativeToolInfo, RestoreSpec,
};
use dbstudio_core::{ConfirmationGrant, Error, JobId, Secret};
use dbstudio_store::SecretChange;
use tauri::State;
use uuid::Uuid;

use super::blocking;
use crate::grants::Purpose;
use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

fn unsupported<T>(what: &str) -> Result<T> {
    Err(Error::Unsupported(what.into()))
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

pub(crate) async fn jobs_list_impl(state: &AppState) -> Result<Vec<BackupJob>> {
    let _ = state;
    unsupported("backup")
}

pub(crate) async fn job_save_impl(
    state: &AppState,
    job: BackupJob,
    passphrase: SecretChange,
) -> Result<BackupJob> {
    let _ = (state, job, passphrase);
    unsupported("backup")
}

pub(crate) async fn job_delete_impl(state: &AppState, id: Uuid) -> Result<()> {
    let _ = (state, id);
    unsupported("backup")
}

pub(crate) async fn job_run_now_impl(state: &AppState, id: Uuid) -> Result<JobId> {
    let _ = (state, id);
    unsupported("backup")
}

pub(crate) async fn run_manual_impl(
    state: &AppState,
    spec: BackupSpec,
    passphrase: Option<Secret>,
    save_passphrase: bool,
) -> Result<JobId> {
    let _ = (state, spec, passphrase, save_passphrase);
    unsupported("backup")
}

pub(crate) async fn runs_list_impl(
    state: &AppState,
    job_id: Option<Uuid>,
    limit: u32,
    before: Option<String>,
) -> Result<Vec<BackupRun>> {
    let _ = (state, job_id, limit, before);
    unsupported("backup")
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
    let _granted = state.grants.check(Path::new(&path), Purpose::Read)?;
    let _ = passphrase;
    unsupported("backup_verify")
}

pub(crate) async fn restore_start_impl(
    state: &AppState,
    spec: RestoreSpec,
    passphrase: Option<Secret>,
    confirm: Option<ConfirmationGrant>,
) -> Result<JobId> {
    let _ = (state, spec, passphrase, confirm);
    unsupported("restore")
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
    let _ = (cron, timezone, count);
    unsupported("schedule_preview")
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
