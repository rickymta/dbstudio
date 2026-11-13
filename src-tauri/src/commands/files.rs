//! Native file dialogs. Every path a dialog returns is recorded in `PathGrants`; later commands
//! (export, import, backup) accept only granted paths.

use std::path::Path;

use dbstudio_core::{Error, Result};
use tauri::State;

use super::app::{decode_cell, sanitize_file_name, write_atomic};
use super::blocking;
use crate::grants::Purpose;
use crate::host::FileFilter;
use crate::state::AppState;

/// Save dialog; the chosen path is granted for writing. `None` when cancelled.
pub(crate) async fn pick_save_impl(
    state: &AppState,
    suggested_name: String,
    filters: Vec<FileFilter>,
) -> Result<Option<String>> {
    let host = state.host.clone();
    let grants = state.grants.clone();
    blocking(move || {
        let name = sanitize_file_name(&suggested_name);
        let Some(picked) = host.pick_save_path_filtered(&name, &filters)? else {
            return Ok(None);
        };
        let path = grants.grant(&picked, Purpose::Write)?;
        Ok(Some(path.to_string_lossy().into_owned()))
    })
    .await
}

/// Open dialog for a file (granted for reading) or a directory (granted as a directory).
pub(crate) async fn pick_open_impl(
    state: &AppState,
    filters: Vec<FileFilter>,
    directory: bool,
) -> Result<Option<String>> {
    let host = state.host.clone();
    let grants = state.grants.clone();
    blocking(move || {
        let Some(picked) = host.pick_open_path(&filters, directory)? else {
            return Ok(None);
        };
        let purpose = if directory {
            Purpose::Dir
        } else {
            Purpose::Read
        };
        let path = grants.grant(&picked, purpose)?;
        Ok(Some(path.to_string_lossy().into_owned()))
    })
    .await
}

/// Decode base64 bytes and write them to a path chosen in a Save dialog.
pub(crate) async fn save_bytes_impl(
    state: &AppState,
    suggested_name: String,
    data_base64: String,
    filters: Vec<FileFilter>,
) -> Result<Option<String>> {
    let host = state.host.clone();
    let grants = state.grants.clone();
    blocking(move || {
        let bytes = decode_cell(&data_base64)?;
        drop(data_base64);
        let name = sanitize_file_name(&suggested_name);
        let Some(picked) = host.pick_save_path_filtered(&name, &filters)? else {
            return Ok(None);
        };
        let path = grants.grant(&picked, Purpose::Write)?;
        write_atomic(Path::new(&path), &bytes)?;
        Ok(Some(path.to_string_lossy().into_owned()))
    })
    .await
}

type R<T> = std::result::Result<T, Error>;

#[tauri::command]
pub async fn file_pick_save(
    state: State<'_, AppState>,
    suggested_name: String,
    filters: Vec<FileFilter>,
) -> R<Option<String>> {
    tracing::debug!(command = "file_pick_save", "ipc call");
    pick_save_impl(&state, suggested_name, filters).await
}

#[tauri::command]
pub async fn file_pick_open(
    state: State<'_, AppState>,
    filters: Vec<FileFilter>,
    directory: bool,
) -> R<Option<String>> {
    tracing::debug!(command = "file_pick_open", directory, "ipc call");
    pick_open_impl(&state, filters, directory).await
}

#[tauri::command]
pub async fn file_save_bytes(
    state: State<'_, AppState>,
    suggested_name: String,
    data_base64: String,
    filters: Vec<FileFilter>,
) -> R<Option<String>> {
    tracing::debug!(
        command = "file_save_bytes",
        bytes = data_base64.len(),
        "ipc call"
    );
    save_bytes_impl(&state, suggested_name, data_base64, filters).await
}
