//! IPC commands of the SQL debugger (`debug_*`, `routine_breakpoints_*`).
//!
//! Step 0 (contract): every command is a stub that answers `Unsupported`; package T implements
//! them on top of `DebugManager` (`crate::debug`). Command names, arguments and results are
//! frozen; see `docs/design/sql-debugger.md` section 3.3.

#![allow(clippy::too_many_arguments)]

use dbstudio_core::debug::{
    Breakpoint, BreakpointStatus, DebugAction, DebugEndAction, DebugEndReport, DebugStartRequest,
    DebugState, DebugSupport, DebugVariable, EvalResult, RoutineBreakpoint, RoutineRef,
    RoutineSource, SourceRef, WatchResult, WatchSpec,
};
use dbstudio_core::{ConfirmationGrant, Error};
use tauri::State;
use uuid::Uuid;

use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

fn stub<T>(command: &str) -> Result<T> {
    Err(Error::Unsupported(command.into()))
}

pub(crate) async fn debug_support_impl(_state: &AppState, _lease_id: Uuid) -> Result<DebugSupport> {
    stub("debug_support")
}

#[tauri::command]
pub async fn debug_support(state: State<'_, AppState>, lease_id: Uuid) -> Result<DebugSupport> {
    tracing::debug!(command = "debug_support", "ipc call");
    debug_support_impl(&state, lease_id).await
}

pub(crate) async fn debug_start_impl(
    _state: &AppState,
    _request: DebugStartRequest,
) -> Result<DebugState> {
    stub("debug_start")
}

#[tauri::command]
pub async fn debug_start(
    state: State<'_, AppState>,
    request: DebugStartRequest,
) -> Result<DebugState> {
    tracing::debug!(command = "debug_start", "ipc call");
    debug_start_impl(&state, request).await
}

pub(crate) async fn debug_step_impl(
    _state: &AppState,
    _debug_id: Uuid,
    _action: DebugAction,
    _confirm: Option<ConfirmationGrant>,
) -> Result<DebugState> {
    stub("debug_step")
}

#[tauri::command]
pub async fn debug_step(
    state: State<'_, AppState>,
    debug_id: Uuid,
    action: DebugAction,
    confirm: Option<ConfirmationGrant>,
) -> Result<DebugState> {
    tracing::debug!(command = "debug_step", "ipc call");
    debug_step_impl(&state, debug_id, action, confirm).await
}

pub(crate) async fn debug_resume_impl(_state: &AppState, _debug_id: Uuid) -> Result<DebugState> {
    stub("debug_resume")
}

#[tauri::command]
pub async fn debug_resume(state: State<'_, AppState>, debug_id: Uuid) -> Result<DebugState> {
    tracing::debug!(command = "debug_resume", "ipc call");
    debug_resume_impl(&state, debug_id).await
}

pub(crate) async fn debug_pause_impl(_state: &AppState, _debug_id: Uuid) -> Result<DebugState> {
    stub("debug_pause")
}

#[tauri::command]
pub async fn debug_pause(state: State<'_, AppState>, debug_id: Uuid) -> Result<DebugState> {
    tracing::debug!(command = "debug_pause", "ipc call");
    debug_pause_impl(&state, debug_id).await
}

pub(crate) async fn debug_run_to_cursor_impl(
    _state: &AppState,
    _debug_id: Uuid,
    _source: SourceRef,
    _line: u32,
    _force: bool,
) -> Result<DebugState> {
    stub("debug_run_to_cursor")
}

#[tauri::command]
pub async fn debug_run_to_cursor(
    state: State<'_, AppState>,
    debug_id: Uuid,
    source: SourceRef,
    line: u32,
    force: bool,
) -> Result<DebugState> {
    tracing::debug!(command = "debug_run_to_cursor", "ipc call");
    debug_run_to_cursor_impl(&state, debug_id, source, line, force).await
}

pub(crate) async fn debug_stop_impl(
    _state: &AppState,
    _debug_id: Uuid,
    _end: Option<DebugEndAction>,
) -> Result<DebugEndReport> {
    stub("debug_stop")
}

#[tauri::command]
pub async fn debug_stop(
    state: State<'_, AppState>,
    debug_id: Uuid,
    end: Option<DebugEndAction>,
) -> Result<DebugEndReport> {
    tracing::debug!(command = "debug_stop", "ipc call");
    debug_stop_impl(&state, debug_id, end).await
}

pub(crate) async fn debug_restart_impl(
    _state: &AppState,
    _debug_id: Uuid,
    _text: String,
    _end: Option<DebugEndAction>,
) -> Result<DebugState> {
    stub("debug_restart")
}

#[tauri::command]
pub async fn debug_restart(
    state: State<'_, AppState>,
    debug_id: Uuid,
    text: String,
    end: Option<DebugEndAction>,
) -> Result<DebugState> {
    tracing::debug!(command = "debug_restart", "ipc call");
    debug_restart_impl(&state, debug_id, text, end).await
}

pub(crate) async fn debug_evaluate_impl(
    _state: &AppState,
    _debug_id: Uuid,
    _expr: String,
    _frame: Option<u32>,
    _echo: Option<bool>,
) -> Result<EvalResult> {
    stub("debug_evaluate")
}

#[tauri::command]
pub async fn debug_evaluate(
    state: State<'_, AppState>,
    debug_id: Uuid,
    expr: String,
    frame: Option<u32>,
    echo: Option<bool>,
) -> Result<EvalResult> {
    tracing::debug!(command = "debug_evaluate", "ipc call");
    debug_evaluate_impl(&state, debug_id, expr, frame, echo).await
}

pub(crate) async fn debug_set_breakpoints_impl(
    _state: &AppState,
    _debug_id: Uuid,
    _source: SourceRef,
    _breakpoints: Vec<Breakpoint>,
) -> Result<Vec<BreakpointStatus>> {
    stub("debug_set_breakpoints")
}

#[tauri::command]
pub async fn debug_set_breakpoints(
    state: State<'_, AppState>,
    debug_id: Uuid,
    source: SourceRef,
    breakpoints: Vec<Breakpoint>,
) -> Result<Vec<BreakpointStatus>> {
    tracing::debug!(command = "debug_set_breakpoints", "ipc call");
    debug_set_breakpoints_impl(&state, debug_id, source, breakpoints).await
}

pub(crate) async fn debug_set_muted_impl(
    _state: &AppState,
    _debug_id: Uuid,
    _muted: bool,
) -> Result<DebugState> {
    stub("debug_set_muted")
}

#[tauri::command]
pub async fn debug_set_muted(
    state: State<'_, AppState>,
    debug_id: Uuid,
    muted: bool,
) -> Result<DebugState> {
    tracing::debug!(command = "debug_set_muted", "ipc call");
    debug_set_muted_impl(&state, debug_id, muted).await
}

pub(crate) async fn debug_set_variable_impl(
    _state: &AppState,
    _debug_id: Uuid,
    _reference: String,
    _value_text: String,
) -> Result<DebugVariable> {
    stub("debug_set_variable")
}

#[tauri::command]
pub async fn debug_set_variable(
    state: State<'_, AppState>,
    debug_id: Uuid,
    reference: String,
    value_text: String,
) -> Result<DebugVariable> {
    tracing::debug!(command = "debug_set_variable", "ipc call");
    debug_set_variable_impl(&state, debug_id, reference, value_text).await
}

pub(crate) async fn debug_variable_children_impl(
    _state: &AppState,
    _debug_id: Uuid,
    _reference: String,
) -> Result<Vec<DebugVariable>> {
    stub("debug_variable_children")
}

#[tauri::command]
pub async fn debug_variable_children(
    state: State<'_, AppState>,
    debug_id: Uuid,
    reference: String,
) -> Result<Vec<DebugVariable>> {
    tracing::debug!(command = "debug_variable_children", "ipc call");
    debug_variable_children_impl(&state, debug_id, reference).await
}

pub(crate) async fn debug_watches_set_impl(
    _state: &AppState,
    _debug_id: Uuid,
    _watches: Vec<WatchSpec>,
) -> Result<Vec<WatchResult>> {
    stub("debug_watches_set")
}

#[tauri::command]
pub async fn debug_watches_set(
    state: State<'_, AppState>,
    debug_id: Uuid,
    watches: Vec<WatchSpec>,
) -> Result<Vec<WatchResult>> {
    tracing::debug!(command = "debug_watches_set", "ipc call");
    debug_watches_set_impl(&state, debug_id, watches).await
}

pub(crate) async fn debug_select_frame_impl(
    _state: &AppState,
    _debug_id: Uuid,
    _frame: u32,
) -> Result<DebugState> {
    stub("debug_select_frame")
}

#[tauri::command]
pub async fn debug_select_frame(
    state: State<'_, AppState>,
    debug_id: Uuid,
    frame: u32,
) -> Result<DebugState> {
    tracing::debug!(command = "debug_select_frame", "ipc call");
    debug_select_frame_impl(&state, debug_id, frame).await
}

pub(crate) async fn debug_routine_source_impl(
    _state: &AppState,
    _debug_id: Option<Uuid>,
    _session_id: Uuid,
    _routine: RoutineRef,
) -> Result<RoutineSource> {
    stub("debug_routine_source")
}

#[tauri::command]
pub async fn debug_routine_source(
    state: State<'_, AppState>,
    debug_id: Option<Uuid>,
    session_id: Uuid,
    routine: RoutineRef,
) -> Result<RoutineSource> {
    tracing::debug!(command = "debug_routine_source", "ipc call");
    debug_routine_source_impl(&state, debug_id, session_id, routine).await
}

pub(crate) async fn debug_snapshot_impl(_state: &AppState, _debug_id: Uuid) -> Result<DebugState> {
    stub("debug_snapshot")
}

#[tauri::command]
pub async fn debug_snapshot(state: State<'_, AppState>, debug_id: Uuid) -> Result<DebugState> {
    tracing::debug!(command = "debug_snapshot", "ipc call");
    debug_snapshot_impl(&state, debug_id).await
}

pub(crate) async fn routine_breakpoints_get_impl(
    _state: &AppState,
    _connection_id: Uuid,
) -> Result<Vec<RoutineBreakpoint>> {
    stub("routine_breakpoints_get")
}

#[tauri::command]
pub async fn routine_breakpoints_get(
    state: State<'_, AppState>,
    connection_id: Uuid,
) -> Result<Vec<RoutineBreakpoint>> {
    tracing::debug!(command = "routine_breakpoints_get", "ipc call");
    routine_breakpoints_get_impl(&state, connection_id).await
}

pub(crate) async fn routine_breakpoints_set_impl(
    _state: &AppState,
    _connection_id: Uuid,
    _list: Vec<RoutineBreakpoint>,
) -> Result<()> {
    stub("routine_breakpoints_set")
}

#[tauri::command]
pub async fn routine_breakpoints_set(
    state: State<'_, AppState>,
    connection_id: Uuid,
    list: Vec<RoutineBreakpoint>,
) -> Result<()> {
    tracing::debug!(command = "routine_breakpoints_set", "ipc call");
    routine_breakpoints_set_impl(&state, connection_id, list).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stubs_answer_unsupported() {
        let (state, _secrets, _host) =
            crate::state::testing::test_state_with_host(dbstudio_core::DriverRegistry::new());
        let err = debug_snapshot_impl(&state, Uuid::nil()).await.unwrap_err();
        assert!(matches!(err, Error::Unsupported(m) if m == "debug_snapshot"));
        let err = routine_breakpoints_set_impl(&state, Uuid::nil(), vec![])
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)));
    }
}
