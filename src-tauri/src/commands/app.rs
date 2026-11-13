//! IPC commands exposed to the frontend. Keep them thin: logic belongs in `dbstudio-core`.

use dbstudio_core::DriverDescriptor;
use serde::Serialize;
use tauri::State;

use crate::state::AppState;

/// Basic facts about the running application.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub name: String,
    pub version: String,
    pub os: String,
    pub arch: String,
}

/// Build the [`AppInfo`] for the current process.
pub fn build_app_info(name: &str, version: &str) -> AppInfo {
    AppInfo {
        name: name.to_string(),
        version: version.to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
    }
}

#[tauri::command]
pub fn app_info(app: tauri::AppHandle) -> AppInfo {
    tracing::debug!(command = "app_info", "ipc call");
    let info = app.package_info();
    build_app_info(&info.name, &info.version.to_string())
}

/// Directory holding the application's log files (for display or copying in the UI).
#[tauri::command]
pub fn log_dir(app: tauri::AppHandle) -> Result<String, dbstudio_core::Error> {
    use tauri::Manager;
    tracing::debug!(command = "log_dir", "ipc call");
    app.path()
        .app_log_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| dbstudio_core::Error::Other(format!("log directory unavailable: {e}")))
}

#[tauri::command]
pub fn list_drivers(state: State<'_, AppState>) -> Vec<DriverDescriptor> {
    tracing::debug!(command = "list_drivers", "ipc call");
    state.registry.descriptors()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_info_reports_platform() {
        let info = build_app_info("DBStudio", "0.1.0");
        assert_eq!(info.name, "DBStudio");
        assert!(!info.os.is_empty() && !info.arch.is_empty());
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["version"], "0.1.0");
    }
}
