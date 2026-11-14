//! Commands managing live sessions and the object tree.

use dbstudio_core::{Error, ObjectNode, ObjectPath, ObjectSearchRequest, Secret};
use tauri::State;
use uuid::Uuid;

use super::blocking;
use crate::state::AppState;

pub use crate::session::SessionInfo;

type Result<T> = std::result::Result<T, Error>;

/// Load the profile, pick the password (stored one, else `password`) and open a session.
pub(crate) async fn open_impl(
    state: &AppState,
    connection_id: Uuid,
    password: Option<Secret>,
) -> Result<SessionInfo> {
    let svc = state.connections.clone();
    let (profile, secret) =
        blocking(move || svc.session_credentials(connection_id, password)).await?;
    let ssh = super::ssh::ssh_env(state, &profile.config).await?;
    state
        .sessions
        .open_with_ssh(&profile.config, secret.as_ref(), ssh.as_ref())
        .await
}

/// Reconnect a lost session, keeping its `session_id` (package E). `secret` is the password when
/// it is neither stored in the keychain nor still held in memory.
pub(crate) async fn reconnect_impl(
    state: &AppState,
    session_id: Uuid,
    secret: Option<Secret>,
) -> Result<SessionInfo> {
    let _ = (state, session_id, secret);
    Err(Error::Unsupported("session_reconnect".into()))
}

pub(crate) async fn close_impl(state: &AppState, session_id: Uuid) -> Result<()> {
    state.sessions.close(session_id).await
}

pub(crate) async fn children_impl(
    state: &AppState,
    session_id: Uuid,
    path: ObjectPath,
) -> Result<Vec<ObjectNode>> {
    state.sessions.get(session_id)?.list_children(&path).await
}

pub(crate) async fn ddl_impl(
    state: &AppState,
    session_id: Uuid,
    path: ObjectPath,
) -> Result<String> {
    state.sessions.get(session_id)?.object_ddl(&path).await
}

pub(crate) async fn search_impl(
    state: &AppState,
    session_id: Uuid,
    request: ObjectSearchRequest,
) -> Result<Vec<ObjectNode>> {
    state
        .sessions
        .get(session_id)?
        .search_objects(request)
        .await
}

pub(crate) async fn cancel_impl(state: &AppState, session_id: Uuid, query_id: Uuid) -> Result<()> {
    state.sessions.get(session_id)?.cancel(query_id).await
}

/// Open a session for a saved profile. `password` is only used when the profile does not store
/// its password.
#[tauri::command]
pub async fn session_open(
    state: State<'_, AppState>,
    connection_id: Uuid,
    password: Option<Secret>,
) -> Result<SessionInfo> {
    tracing::debug!(command = "session_open", connection = %connection_id, "ipc call");
    open_impl(&state, connection_id, password).await
}

/// Reconnect a lost session; the session id is kept.
#[tauri::command]
pub async fn session_reconnect(
    state: State<'_, AppState>,
    session_id: Uuid,
    secret: Option<Secret>,
) -> Result<SessionInfo> {
    tracing::debug!(command = "session_reconnect", session = %session_id, "ipc call");
    reconnect_impl(&state, session_id, secret).await
}

/// Close a session.
#[tauri::command]
pub async fn session_close(state: State<'_, AppState>, session_id: Uuid) -> Result<()> {
    tracing::debug!(command = "session_close", session = %session_id, "ipc call");
    close_impl(&state, session_id).await
}

/// Children of a node in the object tree.
#[tauri::command]
pub async fn object_children(
    state: State<'_, AppState>,
    session_id: Uuid,
    path: ObjectPath,
) -> Result<Vec<ObjectNode>> {
    tracing::debug!(command = "object_children", session = %session_id, "ipc call");
    children_impl(&state, session_id, path).await
}

/// DDL of an object.
#[tauri::command]
pub async fn object_ddl(
    state: State<'_, AppState>,
    session_id: Uuid,
    path: ObjectPath,
) -> Result<String> {
    tracing::debug!(command = "object_ddl", session = %session_id, "ipc call");
    ddl_impl(&state, session_id, path).await
}

/// Server-side search of objects by name.
#[tauri::command]
pub async fn object_search(
    state: State<'_, AppState>,
    session_id: Uuid,
    request: ObjectSearchRequest,
) -> Result<Vec<ObjectNode>> {
    tracing::debug!(command = "object_search", session = %session_id, "ipc call");
    search_impl(&state, session_id, request).await
}

/// Cancel a running query or table fetch.
#[tauri::command]
pub async fn query_cancel(
    state: State<'_, AppState>,
    session_id: Uuid,
    query_id: Uuid,
) -> Result<()> {
    tracing::debug!(command = "query_cancel", session = %session_id, query = %query_id, "ipc call");
    cancel_impl(&state, session_id, query_id).await
}
