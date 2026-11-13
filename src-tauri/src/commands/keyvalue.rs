//! Paged object tree and key-value commands (package L).

use dbstudio_core::{
    ChildrenPage, ChildrenPageRequest, ConfirmationGrant, Error, JobId, KeyReadRequest,
    KeyValuePage, ObjectPath,
};
use tauri::State;
use uuid::Uuid;

use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

pub(crate) async fn children_page_impl(
    state: &AppState,
    session_id: Uuid,
    request: ChildrenPageRequest,
) -> Result<ChildrenPage> {
    state
        .sessions
        .get(session_id)?
        .list_children_page(request)
        .await
}

pub(crate) async fn key_read_impl(
    state: &AppState,
    session_id: Uuid,
    request: KeyReadRequest,
) -> Result<KeyValuePage> {
    state.sessions.get(session_id)?.read_key(request).await
}

pub(crate) async fn keys_delete_pattern_impl(
    state: &AppState,
    session_id: Uuid,
    database: ObjectPath,
    pattern: String,
    confirm: Option<ConfirmationGrant>,
) -> Result<JobId> {
    // Package L: production needs a grant from `state.confirm` (digest of the JSON spec), then
    // a `KeyDelete` job in `state.jobs`.
    let _ = (state, session_id, database, pattern, confirm);
    Err(Error::Unsupported("key_value".into()))
}

#[tauri::command]
pub async fn object_children_page(
    state: State<'_, AppState>,
    session_id: Uuid,
    request: ChildrenPageRequest,
) -> Result<ChildrenPage> {
    tracing::debug!(command = "object_children_page", session = %session_id, "ipc call");
    children_page_impl(&state, session_id, request).await
}

#[tauri::command]
pub async fn key_read(
    state: State<'_, AppState>,
    session_id: Uuid,
    request: KeyReadRequest,
) -> Result<KeyValuePage> {
    tracing::debug!(command = "key_read", session = %session_id, "ipc call");
    key_read_impl(&state, session_id, request).await
}

#[tauri::command]
pub async fn keys_delete_pattern(
    state: State<'_, AppState>,
    session_id: Uuid,
    database: ObjectPath,
    pattern: String,
    confirm: Option<ConfirmationGrant>,
) -> Result<JobId> {
    tracing::debug!(command = "keys_delete_pattern", session = %session_id, "ipc call");
    keys_delete_pattern_impl(&state, session_id, database, pattern, confirm).await
}
