//! Commands for browsing table data.

use dbstudio_core::{
    Error, ExecSummary, ObjectPath, QueryEvent, TableCountRequest, TableDataRequest, TableDetails,
};
use tauri::ipc::Channel;
use uuid::Uuid;

type Result<T> = std::result::Result<T, Error>;

fn not_implemented<T>() -> Result<T> {
    Err(Error::Unsupported("not implemented".into()))
}

/// Columns, indexes and foreign keys of a table.
#[tauri::command]
pub async fn table_details(session_id: Uuid, path: ObjectPath) -> Result<TableDetails> {
    let _ = (session_id, path);
    not_implemented()
}

/// Stream a page of table rows through `on_event`.
#[tauri::command]
pub async fn table_fetch(
    session_id: Uuid,
    request: TableDataRequest,
    on_event: Channel<QueryEvent>,
) -> Result<ExecSummary> {
    let _ = (session_id, request, on_event);
    not_implemented()
}

/// Number of rows matching the filters.
#[tauri::command]
pub async fn table_count(session_id: Uuid, request: TableCountRequest) -> Result<u64> {
    let _ = (session_id, request);
    not_implemented()
}
