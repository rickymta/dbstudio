//! Query history, saved queries and editor tab persistence (all in the SQLite store).

use dbstudio_core::{Error, Result};
use dbstudio_store::{
    EditorTabState, HistoryEntry, HistoryQuery, SavedFolder, SavedQueriesSnapshot, SavedQuery,
};
use tauri::State;
use uuid::Uuid;

use super::blocking;
use crate::state::AppState;

pub(crate) async fn history_list_impl(
    state: &AppState,
    query: HistoryQuery,
) -> Result<Vec<HistoryEntry>> {
    let svc = state.connections.clone();
    blocking(move || svc.store().history_list(&query)).await
}

pub(crate) async fn history_delete_impl(state: &AppState, ids: Vec<i64>) -> Result<()> {
    let svc = state.connections.clone();
    blocking(move || {
        for id in ids {
            svc.store().history_delete(id)?;
        }
        Ok(())
    })
    .await
}

/// Delete the history of one connection (all when `None`); returns how many entries went.
pub(crate) async fn history_clear_impl(
    state: &AppState,
    connection_id: Option<Uuid>,
) -> Result<u64> {
    let svc = state.connections.clone();
    blocking(move || svc.store().history_clear(connection_id)).await
}

pub(crate) async fn saved_queries_list_impl(state: &AppState) -> Result<SavedQueriesSnapshot> {
    let svc = state.connections.clone();
    blocking(move || svc.store().saved_snapshot()).await
}

pub(crate) async fn saved_query_save_impl(
    state: &AppState,
    query: SavedQuery,
) -> Result<SavedQuery> {
    let svc = state.connections.clone();
    blocking(move || svc.store().saved_save_query(&query)).await
}

pub(crate) async fn saved_query_delete_impl(state: &AppState, id: Uuid) -> Result<()> {
    let svc = state.connections.clone();
    blocking(move || svc.store().saved_delete_query(id)).await
}

pub(crate) async fn saved_folder_save_impl(
    state: &AppState,
    folder: SavedFolder,
) -> Result<SavedFolder> {
    let svc = state.connections.clone();
    blocking(move || svc.store().saved_save_folder(&folder)).await
}

pub(crate) async fn saved_folder_delete_impl(state: &AppState, id: Uuid) -> Result<()> {
    let svc = state.connections.clone();
    blocking(move || svc.store().saved_delete_folder(id)).await
}

pub(crate) async fn editor_tabs_get_impl(state: &AppState) -> Result<Vec<EditorTabState>> {
    let svc = state.connections.clone();
    blocking(move || svc.store().tabs_get()).await
}

pub(crate) async fn editor_tabs_set_impl(
    state: &AppState,
    tabs: Vec<EditorTabState>,
) -> Result<()> {
    let svc = state.connections.clone();
    blocking(move || svc.store().tabs_set(&tabs)).await
}

type R<T> = std::result::Result<T, Error>;

#[tauri::command]
pub async fn history_list(state: State<'_, AppState>, query: HistoryQuery) -> R<Vec<HistoryEntry>> {
    tracing::debug!(command = "history_list", "ipc call");
    history_list_impl(&state, query).await
}

#[tauri::command]
pub async fn history_delete(state: State<'_, AppState>, ids: Vec<i64>) -> R<()> {
    tracing::debug!(command = "history_delete", count = ids.len(), "ipc call");
    history_delete_impl(&state, ids).await
}

#[tauri::command]
pub async fn history_clear(state: State<'_, AppState>, connection_id: Option<Uuid>) -> R<u64> {
    tracing::debug!(command = "history_clear", "ipc call");
    history_clear_impl(&state, connection_id).await
}

#[tauri::command]
pub async fn saved_queries_list(state: State<'_, AppState>) -> R<SavedQueriesSnapshot> {
    tracing::debug!(command = "saved_queries_list", "ipc call");
    saved_queries_list_impl(&state).await
}

#[tauri::command]
pub async fn saved_query_save(state: State<'_, AppState>, query: SavedQuery) -> R<SavedQuery> {
    tracing::debug!(command = "saved_query_save", "ipc call");
    saved_query_save_impl(&state, query).await
}

#[tauri::command]
pub async fn saved_query_delete(state: State<'_, AppState>, id: Uuid) -> R<()> {
    tracing::debug!(command = "saved_query_delete", "ipc call");
    saved_query_delete_impl(&state, id).await
}

#[tauri::command]
pub async fn saved_folder_save(state: State<'_, AppState>, folder: SavedFolder) -> R<SavedFolder> {
    tracing::debug!(command = "saved_folder_save", "ipc call");
    saved_folder_save_impl(&state, folder).await
}

#[tauri::command]
pub async fn saved_folder_delete(state: State<'_, AppState>, id: Uuid) -> R<()> {
    tracing::debug!(command = "saved_folder_delete", "ipc call");
    saved_folder_delete_impl(&state, id).await
}

#[tauri::command]
pub async fn editor_tabs_get(state: State<'_, AppState>) -> R<Vec<EditorTabState>> {
    tracing::debug!(command = "editor_tabs_get", "ipc call");
    editor_tabs_get_impl(&state).await
}

#[tauri::command]
pub async fn editor_tabs_set(state: State<'_, AppState>, tabs: Vec<EditorTabState>) -> R<()> {
    tracing::debug!(command = "editor_tabs_set", count = tabs.len(), "ipc call");
    editor_tabs_set_impl(&state, tabs).await
}
