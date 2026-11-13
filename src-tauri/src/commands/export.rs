//! Export / import commands and export presets (package G). The engines are scaffolds, so
//! `export_start` and `import_start` answer `Unsupported` until they are filled in.

use std::path::Path;

use dbstudio_core::{ConfirmationGrant, Error, JobId};
use dbstudio_export::{ExportSpec, ImportPreview, ImportPreviewRequest, ImportSpec};
use dbstudio_store::ExportPreset;
use tauri::State;
use uuid::Uuid;

use super::blocking;
use crate::grants::Purpose;
use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

pub(crate) async fn start_impl(state: &AppState, spec: ExportSpec) -> Result<JobId> {
    // Package G: check `spec.destination` against `state.grants`, then `state.jobs.spawn(..)`
    // with `dbstudio_export::run_export`.
    let _ = (state, spec);
    Err(Error::Unsupported("export".into()))
}

pub(crate) async fn presets_list_impl(
    state: &AppState,
    connection_id: Option<Uuid>,
) -> Result<Vec<ExportPreset>> {
    let svc = state.connections.clone();
    blocking(move || svc.store().presets_list(connection_id)).await
}

pub(crate) async fn preset_save_impl(
    state: &AppState,
    preset: ExportPreset,
) -> Result<ExportPreset> {
    let svc = state.connections.clone();
    blocking(move || svc.store().presets_save(&preset)).await
}

pub(crate) async fn preset_delete_impl(state: &AppState, id: Uuid) -> Result<()> {
    let svc = state.connections.clone();
    blocking(move || svc.store().presets_delete(id)).await
}

/// Only files the user picked in a dialog can be previewed.
pub(crate) async fn import_preview_impl(
    state: &AppState,
    request: ImportPreviewRequest,
) -> Result<ImportPreview> {
    state
        .grants
        .check(Path::new(&request.file), Purpose::Read)?;
    dbstudio_export::preview_import(request).await
}

pub(crate) async fn import_start_impl(
    state: &AppState,
    spec: ImportSpec,
    confirm: Option<ConfirmationGrant>,
) -> Result<JobId> {
    // Package G: check the source file against `state.grants`; script/overwrite imports into a
    // production connection go through `state.confirm`.
    let _ = (state, spec, confirm);
    Err(Error::Unsupported("import".into()))
}

#[tauri::command]
pub async fn export_start(state: State<'_, AppState>, spec: ExportSpec) -> Result<JobId> {
    tracing::debug!(command = "export_start", "ipc call");
    start_impl(&state, spec).await
}

#[tauri::command]
pub async fn export_presets_list(
    state: State<'_, AppState>,
    connection_id: Option<Uuid>,
) -> Result<Vec<ExportPreset>> {
    tracing::debug!(command = "export_presets_list", "ipc call");
    presets_list_impl(&state, connection_id).await
}

#[tauri::command]
pub async fn export_preset_save(
    state: State<'_, AppState>,
    preset: ExportPreset,
) -> Result<ExportPreset> {
    tracing::debug!(command = "export_preset_save", "ipc call");
    preset_save_impl(&state, preset).await
}

#[tauri::command]
pub async fn export_preset_delete(state: State<'_, AppState>, id: Uuid) -> Result<()> {
    tracing::debug!(command = "export_preset_delete", "ipc call");
    preset_delete_impl(&state, id).await
}

#[tauri::command]
pub async fn import_preview(
    state: State<'_, AppState>,
    request: ImportPreviewRequest,
) -> Result<ImportPreview> {
    tracing::debug!(command = "import_preview", "ipc call");
    import_preview_impl(&state, request).await
}

#[tauri::command]
pub async fn import_start(
    state: State<'_, AppState>,
    spec: ImportSpec,
    confirm: Option<ConfirmationGrant>,
) -> Result<JobId> {
    tracing::debug!(command = "import_start", "ipc call");
    import_start_impl(&state, spec, confirm).await
}
