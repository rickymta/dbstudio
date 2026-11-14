//! Background behaviour settings (package H): close-to-tray and autostart.

use dbstudio_core::{Error, Result};
use dbstudio_store::CloseBehavior;
use serde::{Deserialize, Serialize};
use tauri::State;

use super::blocking;
#[cfg(any(test, feature = "dev-bridge"))]
use crate::background::NoAutostart;
use crate::background::{Autostart, TauriAutostart};
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

/// Settings as stored, with the autostart state read from the operating system.
pub(crate) async fn get_with(
    state: &AppState,
    autostart: &dyn Autostart,
) -> Result<BackgroundSettings> {
    let svc = state.connections.clone();
    let settings = blocking(move || svc.settings()).await?;
    Ok(BackgroundSettings {
        close_to_tray: settings.close_to_tray,
        autostart: autostart.is_enabled()?,
    })
}

/// Persist `close_to_tray` and switch launch-at-login on or off.
pub(crate) async fn set_with(
    state: &AppState,
    settings: BackgroundSettings,
    autostart: &dyn Autostart,
) -> Result<BackgroundSettings> {
    let svc = state.connections.clone();
    blocking(move || {
        let mut current = svc.settings()?;
        current.close_to_tray = settings.close_to_tray;
        svc.set_settings(current)
    })
    .await?;
    if autostart.is_enabled()? != settings.autostart {
        autostart.set_enabled(settings.autostart)?;
    }
    get_with(state, autostart).await
}

/// [`get_with`] without autostart support (dev bridge and tests).
#[cfg(any(test, feature = "dev-bridge"))]
pub(crate) async fn get_impl(state: &AppState) -> Result<BackgroundSettings> {
    get_with(state, &NoAutostart).await
}

/// [`set_with`] without autostart support (dev bridge and tests).
#[cfg(any(test, feature = "dev-bridge"))]
pub(crate) async fn set_impl(
    state: &AppState,
    settings: BackgroundSettings,
) -> Result<BackgroundSettings> {
    set_with(state, settings, &NoAutostart).await
}

type R<T> = std::result::Result<T, Error>;

#[tauri::command]
pub async fn background_get(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> R<BackgroundSettings> {
    tracing::debug!(command = "background_get", "ipc call");
    get_with(&state, &TauriAutostart(app)).await
}

#[tauri::command]
pub async fn background_set(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    settings: BackgroundSettings,
) -> R<BackgroundSettings> {
    tracing::debug!(command = "background_set", "ipc call");
    set_with(&state, settings, &TauriAutostart(app)).await
}
