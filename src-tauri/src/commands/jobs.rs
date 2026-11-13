//! Background job registry commands (package G).

use dbstudio_core::{Error, JobId, JobInfo};
use tauri::State;

use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

pub(crate) fn list_impl(state: &AppState) -> Vec<JobInfo> {
    state.jobs.list()
}

pub(crate) fn get_impl(state: &AppState, job_id: JobId) -> Result<JobInfo> {
    state.jobs.get(job_id)
}

pub(crate) fn cancel_impl(state: &AppState, job_id: JobId) -> Result<()> {
    state.jobs.cancel(job_id)
}

pub(crate) fn clear_finished_impl(state: &AppState) -> usize {
    state.jobs.clear_finished()
}

#[tauri::command]
pub fn jobs_list(state: State<'_, AppState>) -> Vec<JobInfo> {
    tracing::debug!(command = "jobs_list", "ipc call");
    list_impl(&state)
}

#[tauri::command]
pub fn job_get(state: State<'_, AppState>, job_id: JobId) -> Result<JobInfo> {
    tracing::debug!(command = "job_get", job = %job_id, "ipc call");
    get_impl(&state, job_id)
}

#[tauri::command]
pub fn job_cancel(state: State<'_, AppState>, job_id: JobId) -> Result<()> {
    tracing::debug!(command = "job_cancel", job = %job_id, "ipc call");
    cancel_impl(&state, job_id)
}

/// Remove finished jobs; returns how many.
#[tauri::command]
pub fn jobs_clear_finished(state: State<'_, AppState>) -> usize {
    tracing::debug!(command = "jobs_clear_finished", "ipc call");
    clear_finished_impl(&state)
}
