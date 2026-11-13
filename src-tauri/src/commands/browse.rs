//! Commands for browsing table data.

use dbstudio_core::{
    Error, ExecSummary, ObjectPath, QueryEvent, TableCountRequest, TableDataRequest, TableDetails,
};
use tauri::State;
use tauri::ipc::Channel;
use uuid::Uuid;

use crate::sink::EventSink;
use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

pub(crate) async fn details_impl(
    state: &AppState,
    session_id: Uuid,
    path: ObjectPath,
) -> Result<TableDetails> {
    state.sessions.get(session_id)?.table_details(&path).await
}

/// Stream a page of rows through `emit`; `emit` returns `false` when the receiver is gone, which
/// stops the driver with [`Error::Cancelled`].
pub(crate) async fn fetch_impl<F>(
    state: &AppState,
    session_id: Uuid,
    request: TableDataRequest,
    emit: F,
) -> Result<ExecSummary>
where
    F: FnMut(QueryEvent) -> bool + Send,
{
    let session = state.sessions.get(session_id)?;
    let mut sink = EventSink::new(emit);
    session.fetch_table(request, &mut sink).await
}

pub(crate) async fn count_impl(
    state: &AppState,
    session_id: Uuid,
    request: TableCountRequest,
) -> Result<u64> {
    state.sessions.get(session_id)?.count_table(request).await
}

/// Columns, indexes and foreign keys of a table.
#[tauri::command]
pub async fn table_details(
    state: State<'_, AppState>,
    session_id: Uuid,
    path: ObjectPath,
) -> Result<TableDetails> {
    tracing::debug!(command = "table_details", session = %session_id, "ipc call");
    details_impl(&state, session_id, path).await
}

/// Stream a page of table rows through `on_event`.
#[tauri::command]
pub async fn table_fetch(
    state: State<'_, AppState>,
    session_id: Uuid,
    request: TableDataRequest,
    on_event: Channel<QueryEvent>,
) -> Result<ExecSummary> {
    tracing::debug!(command = "table_fetch", session = %session_id, query = %request.query_id, "ipc call");
    fetch_impl(&state, session_id, request, move |event| {
        on_event.send(event).is_ok()
    })
    .await
}

/// Number of rows matching the filters.
#[tauri::command]
pub async fn table_count(
    state: State<'_, AppState>,
    session_id: Uuid,
    request: TableCountRequest,
) -> Result<u64> {
    tracing::debug!(command = "table_count", session = %session_id, "ipc call");
    count_impl(&state, session_id, request).await
}
