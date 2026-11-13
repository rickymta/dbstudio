//! Data editing commands (package D): editability, change preview and apply.

use dbstudio_core::{ApplyResult, ChangeSet, Editability, Error, ObjectPath};
use tauri::State;
use uuid::Uuid;

use crate::guard;
use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

pub(crate) async fn editability_impl(
    state: &AppState,
    session_id: Uuid,
    path: ObjectPath,
) -> Result<Editability> {
    state.sessions.get(session_id)?.editability(&path).await
}

pub(crate) async fn preview_impl(
    state: &AppState,
    session_id: Uuid,
    changes: ChangeSet,
) -> Result<Vec<String>> {
    let (session, config) = state.sessions.get_with_config(session_id)?;
    guard::ensure_writable(&config, "data editing")?;
    session.preview_changes(&changes).await
}

pub(crate) async fn apply_impl(
    state: &AppState,
    session_id: Uuid,
    changes: ChangeSet,
) -> Result<ApplyResult> {
    let (session, config) = state.sessions.get_with_config(session_id)?;
    guard::ensure_writable(&config, "data editing")?;
    session.apply_changes(changes).await
}

#[tauri::command]
pub async fn table_editability(
    state: State<'_, AppState>,
    session_id: Uuid,
    path: ObjectPath,
) -> Result<Editability> {
    tracing::debug!(command = "table_editability", session = %session_id, "ipc call");
    editability_impl(&state, session_id, path).await
}

#[tauri::command]
pub async fn changes_preview(
    state: State<'_, AppState>,
    session_id: Uuid,
    changes: ChangeSet,
) -> Result<Vec<String>> {
    tracing::debug!(command = "changes_preview", session = %session_id, "ipc call");
    preview_impl(&state, session_id, changes).await
}

#[tauri::command]
pub async fn changes_apply(
    state: State<'_, AppState>,
    session_id: Uuid,
    changes: ChangeSet,
) -> Result<ApplyResult> {
    tracing::debug!(command = "changes_apply", session = %session_id, "ipc call");
    apply_impl(&state, session_id, changes).await
}
