//! Application-level commands: info, drivers, settings, logs and saving cell values.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use dbstudio_core::{DriverDescriptor, Error, Result};
use dbstudio_store::AppSettings;
use serde::Serialize;
use tauri::State;
use uuid::Uuid;

use super::blocking;
use crate::logging;
use crate::state::AppState;

/// Largest value `save_cell_value` writes (bytes).
pub const MAX_SAVE_BYTES: usize = 256 * 1024 * 1024;

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

pub(crate) fn app_info_impl(state: &AppState) -> AppInfo {
    let (name, version) = state.host.app_identity();
    build_app_info(&name, &version)
}

pub(crate) fn list_drivers_impl(state: &AppState) -> Vec<DriverDescriptor> {
    state.registry.descriptors()
}

pub(crate) fn log_dir_impl(state: &AppState) -> Result<String> {
    Ok(state.host.log_dir()?.to_string_lossy().into_owned())
}

pub(crate) async fn open_log_dir_impl(state: &AppState) -> Result<()> {
    let dir = state.host.log_dir()?;
    let host = state.host.clone();
    blocking(move || {
        std::fs::create_dir_all(&dir)?;
        host.open_dir(&dir)
    })
    .await
}

pub(crate) async fn settings_get_impl(state: &AppState) -> Result<AppSettings> {
    let svc = state.connections.clone();
    blocking(move || svc.settings()).await
}

/// Normalize and store the settings, then apply the log level right away.
pub(crate) async fn settings_set_impl(
    state: &AppState,
    settings: AppSettings,
) -> Result<AppSettings> {
    let svc = state.connections.clone();
    let stored = blocking(move || svc.set_settings(settings)).await?;
    if logging::set_level(stored.log_level) {
        tracing::info!(level = ?stored.log_level, "log level changed");
    }
    Ok(stored)
}

/// Make a user-visible file name from an arbitrary suggestion: no path separators, control or
/// reserved characters, no leading dots, bounded length, never empty.
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace() || c == '_');
    let limited: String = trimmed.chars().take(120).collect();
    if limited.is_empty() {
        "value.bin".to_string()
    } else {
        limited
    }
}

/// Decode standard base64, refusing anything larger than [`MAX_SAVE_BYTES`].
pub fn decode_cell(data_base64: &str) -> Result<Vec<u8>> {
    // 4 characters encode 3 bytes; reject before allocating.
    if data_base64.len() / 4 * 3 > MAX_SAVE_BYTES + 3 {
        return Err(Error::Other(format!(
            "value is larger than {} MB",
            MAX_SAVE_BYTES >> 20
        )));
    }
    let bytes = STANDARD
        .decode(data_base64.trim())
        .map_err(|e| Error::Other(format!("invalid base64 data: {e}")))?;
    if bytes.len() > MAX_SAVE_BYTES {
        return Err(Error::Other(format!(
            "value is larger than {} MB",
            MAX_SAVE_BYTES >> 20
        )));
    }
    Ok(bytes)
}

/// Write `bytes` to `path` through a temporary file in the same directory, then rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| Error::Other("save path has no file name".into()))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.tmp", Uuid::new_v4().simple()));
    let tmp = path.with_file_name(tmp_name);
    let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Ask the user where to save (native dialog, chosen by Rust) and write the decoded bytes.
/// Returns the saved path, or `None` when the dialog was cancelled.
pub(crate) async fn save_cell_value_impl(
    state: &AppState,
    suggested_name: String,
    data_base64: String,
) -> Result<Option<String>> {
    let host = state.host.clone();
    blocking(move || {
        let bytes = decode_cell(&data_base64)?;
        drop(data_base64);
        let name = sanitize_file_name(&suggested_name);
        let Some(path) = host.pick_save_path(&name)? else {
            return Ok(None);
        };
        write_atomic(&path, &bytes)?;
        tracing::info!(bytes = bytes.len(), "cell value saved");
        Ok(Some(path.to_string_lossy().into_owned()))
    })
    .await
}

#[tauri::command]
pub fn app_info(state: State<'_, AppState>) -> AppInfo {
    tracing::debug!(command = "app_info", "ipc call");
    app_info_impl(&state)
}

#[tauri::command]
pub fn log_dir(state: State<'_, AppState>) -> Result<String> {
    tracing::debug!(command = "log_dir", "ipc call");
    log_dir_impl(&state)
}

/// Open the log directory in the OS file manager.
#[tauri::command]
pub async fn open_log_dir(state: State<'_, AppState>) -> Result<()> {
    tracing::debug!(command = "open_log_dir", "ipc call");
    open_log_dir_impl(&state).await
}

#[tauri::command]
pub fn list_drivers(state: State<'_, AppState>) -> Vec<DriverDescriptor> {
    tracing::debug!(command = "list_drivers", "ipc call");
    list_drivers_impl(&state)
}

/// Current application settings.
#[tauri::command]
pub async fn settings_get(state: State<'_, AppState>) -> Result<AppSettings> {
    tracing::debug!(command = "settings_get", "ipc call");
    settings_get_impl(&state).await
}

/// Store settings; returns the normalized value that was saved.
#[tauri::command]
pub async fn settings_set(
    state: State<'_, AppState>,
    settings: AppSettings,
) -> Result<AppSettings> {
    tracing::debug!(command = "settings_set", "ipc call");
    settings_set_impl(&state, settings).await
}

/// Save bytes (base64) to a file the user picks in a native Save dialog.
#[tauri::command]
pub async fn save_cell_value(
    state: State<'_, AppState>,
    suggested_name: String,
    data_base64: String,
) -> Result<Option<String>> {
    tracing::debug!(
        command = "save_cell_value",
        bytes = data_base64.len(),
        "ipc call"
    );
    save_cell_value_impl(&state, suggested_name, data_base64).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testing::test_state_with_host;
    use dbstudio_core::DriverRegistry;

    #[test]
    fn app_info_reports_platform() {
        let info = build_app_info("DBStudio", "0.1.0");
        assert_eq!(info.name, "DBStudio");
        assert!(!info.os.is_empty() && !info.arch.is_empty());
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["version"], "0.1.0");
    }

    #[test]
    fn file_names_are_sanitized() {
        assert_eq!(sanitize_file_name("report.csv"), "report.csv");
        assert_eq!(sanitize_file_name("../../etc/passwd"), "etc_passwd");
        assert_eq!(sanitize_file_name("a\\b:c*d?.txt"), "a_b_c_d_.txt");
        assert_eq!(sanitize_file_name("  ..hidden  "), "hidden");
        assert_eq!(
            sanitize_file_name("line\nbreak\u{7}.bin"),
            "line_break_.bin"
        );
        assert_eq!(sanitize_file_name(""), "value.bin");
        assert_eq!(sanitize_file_name("///"), "value.bin");
        assert_eq!(sanitize_file_name(&"x".repeat(500)).chars().count(), 120);
        assert_eq!(sanitize_file_name("日本語.txt"), "日本語.txt");
    }

    #[test]
    fn decode_validates_input() {
        assert_eq!(decode_cell("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_cell("").unwrap(), b"");
        assert!(decode_cell("***").is_err());
        let too_big = "A".repeat((MAX_SAVE_BYTES / 3 + 10) * 4);
        assert!(
            decode_cell(&too_big)
                .unwrap_err()
                .to_string()
                .contains("larger")
        );
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert!(write_atomic(&dir.path().join("missing/out.bin"), b"x").is_err());
    }

    #[tokio::test]
    async fn save_cell_value_uses_the_dialog_choice_only() {
        let (state, _, host) = test_state_with_host(DriverRegistry::new());
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("picked.bin");
        *host.save_to.lock().unwrap() = Some(target.clone());
        let saved = save_cell_value_impl(&state, "../evil/name.bin".into(), "aGk=".into())
            .await
            .unwrap();
        assert_eq!(saved.as_deref(), Some(target.to_string_lossy().as_ref()));
        assert_eq!(std::fs::read(&target).unwrap(), b"hi");
        assert_eq!(host.suggested.lock().unwrap().as_slice(), ["evil_name.bin"]);

        // Cancelled dialog: nothing written, None returned.
        *host.save_to.lock().unwrap() = None;
        assert_eq!(
            save_cell_value_impl(&state, "x".into(), "aGk=".into())
                .await
                .unwrap(),
            None
        );
        // Bad data fails before any dialog opens.
        let before = host.suggested.lock().unwrap().len();
        assert!(
            save_cell_value_impl(&state, "x".into(), "!!".into())
                .await
                .is_err()
        );
        assert_eq!(host.suggested.lock().unwrap().len(), before);
    }

    #[tokio::test]
    async fn settings_commands_persist_and_clamp() {
        let (state, _, _) = test_state_with_host(DriverRegistry::new());
        assert_eq!(
            settings_get_impl(&state).await.unwrap(),
            AppSettings::default()
        );
        let stored = settings_set_impl(
            &state,
            AppSettings {
                grid_max_rows: 1,
                ..AppSettings::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(stored.grid_max_rows, 1_000);
        assert_eq!(settings_get_impl(&state).await.unwrap(), stored);
    }

    #[tokio::test]
    async fn log_dir_commands_use_the_host() {
        let (state, _, host) = test_state_with_host(DriverRegistry::new());
        assert_eq!(app_info_impl(&state).name, "DBStudio");
        assert_eq!(
            log_dir_impl(&state).unwrap(),
            host.log_dir.to_string_lossy()
        );
        let dir = tempfile::tempdir().unwrap();
        let (mut state, _, _) = test_state_with_host(DriverRegistry::new());
        let fake = std::sync::Arc::new(crate::state::testing::FakeHost {
            log_dir: dir.path().join("logs"),
            ..Default::default()
        });
        state.host = fake.clone();
        open_log_dir_impl(&state).await.unwrap();
        assert_eq!(
            fake.opened.lock().unwrap().as_slice(),
            [dir.path().join("logs")]
        );
        assert!(dir.path().join("logs").is_dir());
    }
}
