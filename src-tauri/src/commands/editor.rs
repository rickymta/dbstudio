//! SQL editor / console commands: script analysis, guarded execution, leases and SQL files.
//!
//! Package B fills in history recording and the real lease drivers; `query_execute` already runs
//! the whole guard flow (design 1.5) and the lease commands forward to `dbstudio_core::Lease`.

use std::path::Path;

use dbstudio_core::{
    Error, ExecuteRequest, ExecuteResult, LeaseCloseReport, LeaseContext, LeaseOptions, LeaseState,
    QueryEvent, Result, ScriptAnalysis, TxOptions,
};
use dbstudio_sql::{OffsetMap, Vocabulary};
use serde::Serialize;
use tauri::State;
use tauri::ipc::Channel;
use uuid::Uuid;

use super::app::write_atomic;
use super::blocking;
use crate::grants::Purpose;
use crate::guard::{self, GuardInput};
use crate::host::FileFilter;
use crate::lease::{LeaseInfo, MAX_LEASES_PER_SESSION};
use crate::sink::EventSink;
use crate::state::AppState;

/// Largest SQL file `sql_file_open` reads (bytes).
pub const MAX_SQL_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// Result of `sql_file_open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlFile {
    /// Path the user picked.
    pub path: String,
    /// File content (UTF-8, BOM removed).
    pub text: String,
}

fn sql_filters() -> Vec<FileFilter> {
    vec![FileFilter {
        name: "SQL".into(),
        extensions: vec!["sql".into(), "txt".into()],
    }]
}

pub(crate) fn analyze_impl(
    state: &AppState,
    session_id: Uuid,
    text: &str,
) -> Result<ScriptAnalysis> {
    let session = state.sessions.get(session_id)?;
    Ok(dbstudio_sql::analyze(session.script_dialect(), text))
}

pub(crate) fn vocabulary_impl(state: &AppState, session_id: Uuid) -> Result<Vocabulary> {
    let session = state.sessions.get(session_id)?;
    Ok(dbstudio_sql::vocabulary(session.script_dialect()))
}

/// Wrap `emit` so `StatementStart` ranges (positions in the bound text) are reported in the text
/// the user typed.
fn mapped_emit<F>(map: OffsetMap, mut emit: F) -> impl FnMut(QueryEvent) -> bool + Send
where
    F: FnMut(QueryEvent) -> bool + Send,
{
    move |event| match event {
        QueryEvent::StatementStart { mut start } => {
            start.range.start = map.to_original(start.range.start);
            start.range.end = map.to_original(start.range.end);
            emit(QueryEvent::StatementStart { start })
        }
        other => emit(other),
    }
}

/// Error positions come back in the bound text; the editor wants the typed text.
fn map_error(map: &OffsetMap, error: Error) -> Error {
    match error {
        Error::Query {
            message,
            position: Some(p),
        } => Error::Query {
            message,
            position: Some(map.to_original(p)),
        },
        other => other,
    }
}

/// Run a script on a pooled connection of the session: guard (steps 1-4), then the driver with
/// events streamed through `emit`.
pub(crate) async fn execute_impl<F>(
    state: &AppState,
    session_id: Uuid,
    request: ExecuteRequest,
    emit: F,
) -> Result<ExecuteResult>
where
    F: FnMut(QueryEvent) -> bool + Send,
{
    let (session, config) = state.sessions.get_with_config(session_id)?;
    let ExecuteRequest {
        mut query,
        params,
        confirm,
        record_history: _, // history recording is package B's
    } = request;
    let target = format!("session:{session_id}");
    let confirm_text = guard::confirm_text_for(&config, None);
    let (read_only, production) = guard::policy_flags(&config);
    let guarded = guard::run_guard(
        &state.confirm,
        &GuardInput {
            dialect: session.script_dialect(),
            text: &query.text,
            params: &params,
            read_only,
            production,
            confirm_text: &confirm_text,
            confirm: confirm.as_ref(),
            target_id: &target,
        },
    )?;
    let map = guarded.bound.map;
    query.text = guarded.bound.text;
    let mut sink = EventSink::new(mapped_emit(map.clone(), emit));
    let summary = session
        .execute(query, &mut sink)
        .await
        .map_err(|e| map_error(&map, e))?;
    Ok(ExecuteResult {
        summary,
        state: None,
        history_id: None,
    })
}

pub(crate) async fn lease_open_impl(
    state: &AppState,
    session_id: Uuid,
    options: LeaseOptions,
) -> Result<LeaseInfo> {
    let session = state.sessions.get(session_id)?;
    if state.leases.count_for_session(session_id) >= MAX_LEASES_PER_SESSION {
        return Err(Error::Busy(format!(
            "a session can hold at most {MAX_LEASES_PER_SESSION} leases"
        )));
    }
    let lease = session.open_lease(options).await?;
    state.leases.register(session_id, lease)
}

pub(crate) async fn lease_close_impl(state: &AppState, lease_id: Uuid) -> Result<LeaseCloseReport> {
    state.leases.close(lease_id).await
}

/// Like [`execute_impl`] on a lease; the confirmation text follows the lease's current database.
pub(crate) async fn lease_execute_impl<F>(
    state: &AppState,
    lease_id: Uuid,
    request: ExecuteRequest,
    emit: F,
) -> Result<ExecuteResult>
where
    F: FnMut(QueryEvent) -> bool + Send,
{
    let (session_id, lease) = state.leases.get_with_session(lease_id)?;
    let (session, config) = state.sessions.get_with_config(session_id)?;
    let ExecuteRequest {
        mut query,
        params,
        confirm,
        record_history: _,
    } = request;
    let target = format!("lease:{lease_id}");
    let lease_db = lease.state().database;
    let confirm_text = guard::confirm_text_for(&config, lease_db.as_deref());
    let (read_only, production) = guard::policy_flags(&config);
    let guarded = guard::run_guard(
        &state.confirm,
        &GuardInput {
            dialect: session.script_dialect(),
            text: &query.text,
            params: &params,
            read_only,
            production,
            confirm_text: &confirm_text,
            confirm: confirm.as_ref(),
            target_id: &target,
        },
    )?;
    let map = guarded.bound.map;
    query.text = guarded.bound.text;
    let mut sink = EventSink::new(mapped_emit(map.clone(), emit));
    let summary = lease
        .execute(query, &mut sink)
        .await
        .map_err(|e| map_error(&map, e))?;
    state.leases.notify_state(lease_id);
    Ok(ExecuteResult {
        summary,
        state: Some(lease.state()),
        history_id: None,
    })
}

pub(crate) async fn lease_fetch_more_impl<F>(
    state: &AppState,
    lease_id: Uuid,
    query_id: Uuid,
    max_rows: u64,
    batch_size: Option<u32>,
    emit: F,
) -> Result<ExecuteResult>
where
    F: FnMut(QueryEvent) -> bool + Send,
{
    let lease = state.leases.get(lease_id)?;
    let mut sink = EventSink::new(emit);
    let summary = lease
        .fetch_more(query_id, max_rows, batch_size, &mut sink)
        .await?;
    Ok(ExecuteResult {
        summary,
        state: Some(lease.state()),
        history_id: None,
    })
}

pub(crate) async fn lease_discard_result_impl(
    state: &AppState,
    lease_id: Uuid,
) -> Result<LeaseState> {
    let lease = state.leases.get(lease_id)?;
    lease.discard_result().await?;
    Ok(lease.state())
}

pub(crate) async fn lease_cancel_impl(
    state: &AppState,
    lease_id: Uuid,
    query_id: Uuid,
) -> Result<()> {
    state.leases.get(lease_id)?.cancel(query_id).await
}

pub(crate) async fn lease_begin_impl(
    state: &AppState,
    lease_id: Uuid,
    options: TxOptions,
) -> Result<LeaseState> {
    let lease = state.leases.get(lease_id)?;
    lease.begin(options).await?;
    Ok(lease.state())
}

pub(crate) async fn lease_commit_impl(state: &AppState, lease_id: Uuid) -> Result<LeaseState> {
    let lease = state.leases.get(lease_id)?;
    lease.commit().await?;
    Ok(lease.state())
}

pub(crate) async fn lease_rollback_impl(state: &AppState, lease_id: Uuid) -> Result<LeaseState> {
    let lease = state.leases.get(lease_id)?;
    lease.rollback().await?;
    Ok(lease.state())
}

pub(crate) async fn lease_set_auto_commit_impl(
    state: &AppState,
    lease_id: Uuid,
    auto_commit: bool,
) -> Result<LeaseState> {
    let lease = state.leases.get(lease_id)?;
    lease.set_auto_commit(auto_commit).await?;
    Ok(lease.state())
}

pub(crate) async fn lease_set_context_impl(
    state: &AppState,
    lease_id: Uuid,
    context: LeaseContext,
) -> Result<LeaseState> {
    let lease = state.leases.get(lease_id)?;
    lease.set_context(context).await?;
    Ok(lease.state())
}

pub(crate) fn lease_state_impl(state: &AppState, lease_id: Uuid) -> Result<LeaseState> {
    Ok(state.leases.get(lease_id)?.state())
}

/// Decode file bytes as UTF-8, dropping a BOM.
fn decode_sql(bytes: Vec<u8>) -> Result<String> {
    let text = String::from_utf8(bytes)
        .map_err(|_| Error::Other("the file is not valid UTF-8 text".into()))?;
    Ok(text
        .strip_prefix('\u{feff}')
        .map(str::to_string)
        .unwrap_or(text))
}

/// Native Open dialog, then read the file (<= 50 MB). The path is granted for reading and for
/// saving back.
pub(crate) async fn sql_file_open_impl(state: &AppState) -> Result<Option<SqlFile>> {
    let host = state.host.clone();
    let grants = state.grants.clone();
    blocking(move || {
        let Some(picked) = host.pick_open_path(&sql_filters(), false)? else {
            return Ok(None);
        };
        let path = grants.grant(&picked, Purpose::Read)?;
        grants.grant(&path, Purpose::Write)?;
        let len = std::fs::metadata(&path)?.len();
        if len > MAX_SQL_FILE_BYTES {
            return Err(Error::Other(format!(
                "the file is larger than {} MB",
                MAX_SQL_FILE_BYTES >> 20
            )));
        }
        let text = decode_sql(std::fs::read(&path)?)?;
        Ok(Some(SqlFile {
            path: path.to_string_lossy().into_owned(),
            text,
        }))
    })
    .await
}

/// Save to `path` (must be granted) or ask for a path; `None` when the dialog was cancelled.
pub(crate) async fn sql_file_save_impl(
    state: &AppState,
    path: Option<String>,
    text: String,
) -> Result<Option<String>> {
    let host = state.host.clone();
    let grants = state.grants.clone();
    blocking(move || {
        let target = match path {
            Some(p) => grants.check(Path::new(&p), Purpose::Write)?,
            None => {
                let Some(picked) = host.pick_save_path_filtered("query.sql", &sql_filters())?
                else {
                    return Ok(None);
                };
                let canon = grants.grant(&picked, Purpose::Write)?;
                grants.grant(&canon, Purpose::Read)?;
                canon
            }
        };
        write_atomic(&target, text.as_bytes())?;
        Ok(Some(target.to_string_lossy().into_owned()))
    })
    .await
}

#[tauri::command]
pub fn script_analyze(
    state: State<'_, AppState>,
    session_id: Uuid,
    text: String,
) -> Result<ScriptAnalysis> {
    tracing::debug!(command = "script_analyze", session = %session_id, "ipc call");
    analyze_impl(&state, session_id, &text)
}

#[tauri::command]
pub fn script_vocabulary(state: State<'_, AppState>, session_id: Uuid) -> Result<Vocabulary> {
    tracing::debug!(command = "script_vocabulary", session = %session_id, "ipc call");
    vocabulary_impl(&state, session_id)
}

/// Run a script on a pooled connection; streams events through `on_event`.
#[tauri::command]
pub async fn query_execute(
    state: State<'_, AppState>,
    session_id: Uuid,
    request: ExecuteRequest,
    on_event: Channel<QueryEvent>,
) -> Result<ExecuteResult> {
    tracing::debug!(command = "query_execute", session = %session_id, query = %request.query.query_id, "ipc call");
    execute_impl(&state, session_id, request, move |event| {
        on_event.send(event).is_ok()
    })
    .await
}

#[tauri::command]
pub async fn lease_open(
    state: State<'_, AppState>,
    session_id: Uuid,
    options: LeaseOptions,
) -> Result<LeaseInfo> {
    tracing::debug!(command = "lease_open", session = %session_id, "ipc call");
    lease_open_impl(&state, session_id, options).await
}

#[tauri::command]
pub async fn lease_close(state: State<'_, AppState>, lease_id: Uuid) -> Result<LeaseCloseReport> {
    tracing::debug!(command = "lease_close", lease = %lease_id, "ipc call");
    lease_close_impl(&state, lease_id).await
}

#[tauri::command]
pub async fn lease_execute(
    state: State<'_, AppState>,
    lease_id: Uuid,
    request: ExecuteRequest,
    on_event: Channel<QueryEvent>,
) -> Result<ExecuteResult> {
    tracing::debug!(command = "lease_execute", lease = %lease_id, query = %request.query.query_id, "ipc call");
    lease_execute_impl(&state, lease_id, request, move |event| {
        on_event.send(event).is_ok()
    })
    .await
}

#[tauri::command]
pub async fn lease_fetch_more(
    state: State<'_, AppState>,
    lease_id: Uuid,
    query_id: Uuid,
    max_rows: u64,
    batch_size: Option<u32>,
    on_event: Channel<QueryEvent>,
) -> Result<ExecuteResult> {
    tracing::debug!(command = "lease_fetch_more", lease = %lease_id, query = %query_id, "ipc call");
    lease_fetch_more_impl(
        &state,
        lease_id,
        query_id,
        max_rows,
        batch_size,
        move |event| on_event.send(event).is_ok(),
    )
    .await
}

#[tauri::command]
pub async fn lease_discard_result(
    state: State<'_, AppState>,
    lease_id: Uuid,
) -> Result<LeaseState> {
    tracing::debug!(command = "lease_discard_result", lease = %lease_id, "ipc call");
    lease_discard_result_impl(&state, lease_id).await
}

#[tauri::command]
pub async fn lease_cancel(
    state: State<'_, AppState>,
    lease_id: Uuid,
    query_id: Uuid,
) -> Result<()> {
    tracing::debug!(command = "lease_cancel", lease = %lease_id, query = %query_id, "ipc call");
    lease_cancel_impl(&state, lease_id, query_id).await
}

#[tauri::command]
pub async fn lease_begin(
    state: State<'_, AppState>,
    lease_id: Uuid,
    options: TxOptions,
) -> Result<LeaseState> {
    tracing::debug!(command = "lease_begin", lease = %lease_id, "ipc call");
    lease_begin_impl(&state, lease_id, options).await
}

#[tauri::command]
pub async fn lease_commit(state: State<'_, AppState>, lease_id: Uuid) -> Result<LeaseState> {
    tracing::debug!(command = "lease_commit", lease = %lease_id, "ipc call");
    lease_commit_impl(&state, lease_id).await
}

#[tauri::command]
pub async fn lease_rollback(state: State<'_, AppState>, lease_id: Uuid) -> Result<LeaseState> {
    tracing::debug!(command = "lease_rollback", lease = %lease_id, "ipc call");
    lease_rollback_impl(&state, lease_id).await
}

#[tauri::command]
pub async fn lease_set_auto_commit(
    state: State<'_, AppState>,
    lease_id: Uuid,
    auto_commit: bool,
) -> Result<LeaseState> {
    tracing::debug!(command = "lease_set_auto_commit", lease = %lease_id, "ipc call");
    lease_set_auto_commit_impl(&state, lease_id, auto_commit).await
}

#[tauri::command]
pub async fn lease_set_context(
    state: State<'_, AppState>,
    lease_id: Uuid,
    context: LeaseContext,
) -> Result<LeaseState> {
    tracing::debug!(command = "lease_set_context", lease = %lease_id, "ipc call");
    lease_set_context_impl(&state, lease_id, context).await
}

#[tauri::command]
pub fn lease_state(state: State<'_, AppState>, lease_id: Uuid) -> Result<LeaseState> {
    tracing::debug!(command = "lease_state", lease = %lease_id, "ipc call");
    lease_state_impl(&state, lease_id)
}

/// Native Open dialog for a SQL file; `None` when cancelled.
#[tauri::command]
pub async fn sql_file_open(state: State<'_, AppState>) -> Result<Option<SqlFile>> {
    tracing::debug!(command = "sql_file_open", "ipc call");
    sql_file_open_impl(&state).await
}

/// Save a SQL file to a granted `path`, or ask for one when omitted.
#[tauri::command]
pub async fn sql_file_save(
    state: State<'_, AppState>,
    path: Option<String>,
    text: String,
) -> Result<Option<String>> {
    tracing::debug!(command = "sql_file_save", "ipc call");
    sql_file_save_impl(&state, path, text).await
}
