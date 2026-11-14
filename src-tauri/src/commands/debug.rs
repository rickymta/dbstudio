//! IPC commands of the SQL debugger (`debug_*`, `routine_breakpoints_*`).
//!
//! Thin layer over `DebugManager` (`crate::debug`) and the store. Command names, arguments and
//! results are frozen; see `docs/design/sql-debugger.md` section 3.3.

#![allow(clippy::too_many_arguments)]

use dbstudio_core::debug::{
    Breakpoint, BreakpointStatus, DebugAction, DebugEndAction, DebugEndReport, DebugStartRequest,
    DebugState, DebugSupport, DebugVariable, EvalResult, RoutineBreakpoint, RoutineRef,
    RoutineSource, SourceRef, WatchResult, WatchSpec,
};
use dbstudio_core::{ConfirmationGrant, Error};
use tauri::State;
use uuid::Uuid;

use super::blocking;
use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

pub(crate) async fn debug_support_impl(state: &AppState, lease_id: Uuid) -> Result<DebugSupport> {
    state.debug.support(state, lease_id).await
}

#[tauri::command]
pub async fn debug_support(state: State<'_, AppState>, lease_id: Uuid) -> Result<DebugSupport> {
    tracing::debug!(command = "debug_support", "ipc call");
    debug_support_impl(&state, lease_id).await
}

pub(crate) async fn debug_start_impl(
    state: &AppState,
    request: DebugStartRequest,
) -> Result<DebugState> {
    state.debug.start(state, request).await
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
    state: &AppState,
    debug_id: Uuid,
    action: DebugAction,
    confirm: Option<ConfirmationGrant>,
) -> Result<DebugState> {
    state.debug.act(debug_id, action, confirm).await
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

pub(crate) async fn debug_resume_impl(state: &AppState, debug_id: Uuid) -> Result<DebugState> {
    state.debug.act(debug_id, DebugAction::Resume, None).await
}

#[tauri::command]
pub async fn debug_resume(state: State<'_, AppState>, debug_id: Uuid) -> Result<DebugState> {
    tracing::debug!(command = "debug_resume", "ipc call");
    debug_resume_impl(&state, debug_id).await
}

pub(crate) async fn debug_pause_impl(state: &AppState, debug_id: Uuid) -> Result<DebugState> {
    state.debug.act(debug_id, DebugAction::Pause, None).await
}

#[tauri::command]
pub async fn debug_pause(state: State<'_, AppState>, debug_id: Uuid) -> Result<DebugState> {
    tracing::debug!(command = "debug_pause", "ipc call");
    debug_pause_impl(&state, debug_id).await
}

pub(crate) async fn debug_run_to_cursor_impl(
    state: &AppState,
    debug_id: Uuid,
    source: SourceRef,
    line: u32,
    force: bool,
) -> Result<DebugState> {
    state
        .debug
        .act(
            debug_id,
            DebugAction::RunToCursor {
                source,
                line,
                force,
            },
            None,
        )
        .await
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
    state: &AppState,
    debug_id: Uuid,
    end: Option<DebugEndAction>,
) -> Result<DebugEndReport> {
    state.debug.stop(debug_id, end).await
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
    state: &AppState,
    debug_id: Uuid,
    text: String,
    end: Option<DebugEndAction>,
) -> Result<DebugState> {
    state.debug.restart(state, debug_id, text, end).await
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
    state: &AppState,
    debug_id: Uuid,
    expr: String,
    frame: Option<u32>,
    echo: Option<bool>,
) -> Result<EvalResult> {
    state
        .debug
        .evaluate(debug_id, &expr, frame, echo.unwrap_or(false))
        .await
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
    state: &AppState,
    debug_id: Uuid,
    source: SourceRef,
    breakpoints: Vec<Breakpoint>,
) -> Result<Vec<BreakpointStatus>> {
    state
        .debug
        .set_breakpoints(debug_id, source, breakpoints)
        .await
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
    state: &AppState,
    debug_id: Uuid,
    muted: bool,
) -> Result<DebugState> {
    state.debug.set_muted(debug_id, muted).await
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
    state: &AppState,
    debug_id: Uuid,
    reference: String,
    value_text: String,
) -> Result<DebugVariable> {
    state
        .debug
        .set_variable(debug_id, &reference, &value_text)
        .await
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
    state: &AppState,
    debug_id: Uuid,
    reference: String,
) -> Result<Vec<DebugVariable>> {
    state.debug.variable_children(debug_id, &reference).await
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
    state: &AppState,
    debug_id: Uuid,
    watches: Vec<WatchSpec>,
) -> Result<Vec<WatchResult>> {
    state.debug.set_watches(debug_id, watches).await
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
    state: &AppState,
    debug_id: Uuid,
    frame: u32,
) -> Result<DebugState> {
    state.debug.select_frame(debug_id, frame).await
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
    state: &AppState,
    debug_id: Option<Uuid>,
    session_id: Uuid,
    routine: RoutineRef,
) -> Result<RoutineSource> {
    state
        .debug
        .routine_source(state, debug_id, session_id, &routine)
        .await
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

pub(crate) async fn debug_snapshot_impl(state: &AppState, debug_id: Uuid) -> Result<DebugState> {
    state.debug.snapshot(debug_id).await
}

#[tauri::command]
pub async fn debug_snapshot(state: State<'_, AppState>, debug_id: Uuid) -> Result<DebugState> {
    tracing::debug!(command = "debug_snapshot", "ipc call");
    debug_snapshot_impl(&state, debug_id).await
}

pub(crate) async fn routine_breakpoints_get_impl(
    state: &AppState,
    connection_id: Uuid,
) -> Result<Vec<RoutineBreakpoint>> {
    let svc = state.connections.clone();
    blocking(move || svc.store().routine_breakpoints_get(connection_id)).await
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
    state: &AppState,
    connection_id: Uuid,
    list: Vec<RoutineBreakpoint>,
) -> Result<()> {
    let svc = state.connections.clone();
    blocking(move || svc.store().routine_breakpoints_set(connection_id, &list)).await
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
