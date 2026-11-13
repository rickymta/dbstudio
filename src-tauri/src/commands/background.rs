//! Background behaviour settings (package H): close-to-tray and autostart.

use dbstudio_core::{Error, Result};
use dbstudio_store::CloseBehavior;
use serde::{Deserialize, Serialize};
use tauri::State;

use super::blocking;
use crate::state::AppState;

/// What the settings page shows and stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundSettings {
    /// What closing the main window does.
    pub close_to_tray: CloseBehavior,
    /// Start DBStudio at login.
    pub autostart: bool,
}

pub(crate) async fn get_impl(state: &AppState) -> Result<BackgroundSettings> {
    let svc = state.connections.clone();
    let settings = blocking(move || svc.settings()).await?;
    Ok(BackgroundSettings {
        close_to_tray: settings.close_to_tray,
        // Package H wires the autostart plugin; until then it is never enabled.
        autostart: false,
    })
}

/// Persist `close_to_tray`. `autostart` is accepted but not applied yet (package H).
pub(crate) async fn set_impl(
    state: &AppState,
    settings: BackgroundSettings,
) -> Result<BackgroundSettings> {
    let svc = state.connections.clone();
    blocking(move || {
        let mut current = svc.settings()?;
        current.close_to_tray = settings.close_to_tray;
        svc.set_settings(current)
    })
    .await?;
    get_impl(state).await
}

type R<T> = std::result::Result<T, Error>;

#[tauri::command]
pub async fn background_get(state: State<'_, AppState>) -> R<BackgroundSettings> {
    tracing::debug!(command = "background_get", "ipc call");
    get_impl(&state).await
}

#[tauri::command]
pub async fn background_set(
    state: State<'_, AppState>,
    settings: BackgroundSettings,
) -> R<BackgroundSettings> {
    tracing::debug!(command = "background_set", "ipc call");
    set_impl(&state, settings).await
}
