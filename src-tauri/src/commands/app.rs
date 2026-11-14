//! Application-level commands: info, drivers, settings, logs and saving cell values.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use dbstudio_core::{DbKind, DriverDescriptor, DriverStatus, Error, Result, UnavailableReason};
use dbstudio_store::AppSettings;
use serde::{Deserialize, Serialize};
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

/// Why a driver is (not) usable on this machine, with a link to instructions when there is one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverDiagnostics {
    pub status: DriverStatus,
    pub message: String,
    pub help_url: Option<String>,
    /// Structured checks behind `message` (empty until package E fills them in).
    #[serde(default)]
    pub checks: Vec<DiagnosticCheck>,
}

/// One line of the diagnostics report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticCheck {
    /// Stable id: `driverRegistered`, `nativeTool:<tool>`, `nativeClient`, `clientVersion`, ...
    pub id: String,
    /// The check passed.
    pub ok: bool,
    /// Measured value (a version, a path), when there is one.
    pub value: Option<String>,
    /// Explanation or hint.
    pub detail: Option<String>,
}

/// Status of the driver of `kind` and an explanation. Oracle has its own report
/// ([`oracle_diagnostics_impl`]).
pub(crate) fn driver_diagnostics_impl(state: &AppState, kind: DbKind) -> Result<DriverDiagnostics> {
    #[cfg(feature = "oracle")]
    if kind == DbKind::Oracle {
        return oracle_diagnostics_impl(state);
    }
    let descriptor = state
        .registry
        .descriptors()
        .into_iter()
        .find(|d| d.kind == kind)
        .ok_or_else(|| Error::NotFound(format!("driver {}", kind.id())))?;
    let name = kind.display_name();
    let (message, help_url) = match descriptor.status {
        DriverStatus::Available => (format!("The {name} driver is available."), None),
        DriverStatus::Planned { phase } => (
            format!("The {name} driver is planned for phase {phase} and not built into this app."),
            None,
        ),
        DriverStatus::Unavailable { reason } => {
            let message = match reason {
                UnavailableReason::NativeClientMissing => {
                    format!("The {name} client library is not installed on this machine.")
                }
                UnavailableReason::NativeClientTooOld => {
                    format!("The installed {name} client library is too old.")
                }
            };
            let help = (kind == DbKind::Oracle).then(|| {
                "https://www.oracle.com/database/technologies/instant-client/downloads.html"
                    .to_string()
            });
            (message, help)
        }
    };
    Ok(DriverDiagnostics {
        status: descriptor.status,
        message,
        help_url,
        checks: Vec::new(),
    })
}

/// Text of the Oracle report: the state of the client library, the places that were searched and
/// how to install it.
#[cfg(feature = "oracle")]
fn oracle_report(d: &dbstudio_driver_oracle::ClientDiagnostics) -> String {
    let mut lines: Vec<String> = Vec::new();
    match (&d.version, d.available) {
        (Some(v), true) => lines.push(format!("Oracle Client library: loaded (version {v}).")),
        _ => lines.push(match d.reason {
            Some(UnavailableReason::NativeClientTooOld) => {
                "Oracle Client library: too old.".to_string()
            }
            _ => "Oracle Client library: not found.".to_string(),
        }),
    }
    match &d.configured_dir {
        Some(dir) => lines.push(format!("Configured folder: {dir}")),
        None => lines.push("Configured folder: none (default locations are searched).".to_string()),
    }
    if let Some(error) = &d.error {
        lines.push(format!("Reported error: {error}"));
    }
    if !d.available && !d.tried_paths.is_empty() {
        lines.push("Locations tried:".to_string());
        lines.extend(d.tried_paths.iter().map(|p| format!("  {p}")));
    }
    lines.push(d.install_hint.clone());
    lines.join("\n")
}

/// Oracle driver status: applies the `oracle_client` setting, probes the client again (so a
/// freshly installed client or a newly chosen folder is picked up without a restart) and
/// explains the result.
#[cfg(feature = "oracle")]
pub(crate) fn oracle_diagnostics_impl(state: &AppState) -> Result<DriverDiagnostics> {
    if let Ok(client) = state.connections.store().get_oracle_client() {
        dbstudio_driver_oracle::configure_client(client.lib_dir.as_deref());
    }
    let report = dbstudio_driver_oracle::client_diagnostics();
    let descriptor = state
        .registry
        .descriptors()
        .into_iter()
        .find(|d| d.kind == DbKind::Oracle)
        .ok_or_else(|| Error::NotFound("driver oracle".into()))?;
    let help_url = (!report.available).then(|| report.download_url.to_string());
    Ok(DriverDiagnostics {
        status: descriptor.status,
        message: oracle_report(&report),
        help_url,
        checks: Vec::new(),
    })
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

/// Longest file name `save_cell_value` suggests, in UTF-8 bytes.
const MAX_NAME_BYTES: usize = 200;

/// Device names Windows refuses as file names (with or without an extension).
fn is_windows_reserved(stem: &str) -> bool {
    let s = stem.to_ascii_uppercase();
    matches!(s.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (s.len() == 4
            && (s.starts_with("COM") || s.starts_with("LPT"))
            && s.as_bytes()[3].is_ascii_digit()
            && s.as_bytes()[3] != b'0')
}

/// Cut `s` to at most `max` bytes without splitting a character.
fn truncate_bytes(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Make a user-visible file name from an arbitrary suggestion: no path separators, control or
/// reserved characters, no leading dots, at most 200 bytes (the extension is kept, characters
/// are never split), no Windows device names, never empty.
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
    if trimmed.is_empty() {
        return "value.bin".to_string();
    }
    // Split off a short extension so truncation never removes it.
    let (stem, ext) = match trimmed.rfind('.') {
        Some(dot) if dot > 0 && trimmed.len() - dot <= 17 => trimmed.split_at(dot),
        _ => (trimmed, ""),
    };
    let stem = truncate_bytes(stem, MAX_NAME_BYTES.saturating_sub(ext.len()));
    let stem = stem.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    // `CON.tar.gz` is reserved too: only the part before the first dot counts.
    let first = stem.split('.').next().unwrap_or(stem);
    let suffix = if is_windows_reserved(first) { "_" } else { "" };
    let (head, tail) = stem.split_at(first.len());
    let result = format!("{head}{suffix}{tail}{ext}");
    if result.is_empty() {
        "value.bin".to_string()
    } else {
        result
    }
}

/// Decode standard base64, refusing anything larger than [`MAX_SAVE_BYTES`].
pub fn decode_cell(data_base64: &str) -> Result<Vec<u8>> {
    // 4 characters encode 3 bytes; reject before allocating.
    if data_base64.len() / 4 * 3 > MAX_SAVE_BYTES + 3 {
        return Err(Error::Invalid(format!(
            "value is larger than {} MB",
            MAX_SAVE_BYTES >> 20
        )));
    }
    let bytes = STANDARD
        .decode(data_base64.trim())
        .map_err(|e| Error::Invalid(format!("invalid base64 data: {e}")))?;
    if bytes.len() > MAX_SAVE_BYTES {
        return Err(Error::Invalid(format!(
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
        .ok_or_else(|| Error::Invalid("save path has no file name".into()))?;
    let _ = name;
    // Short temporary name: the target's own name may already be near the length limit.
    let tmp = path.with_file_name(format!(".dbs-{}.tmp", Uuid::new_v4().simple()));
    let written = (|| {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
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

/// Diagnostics of a driver (`DriverStatus` plus an explanation and a help link).
#[tauri::command]
pub fn driver_diagnostics(state: State<'_, AppState>, kind: DbKind) -> Result<DriverDiagnostics> {
    tracing::debug!(command = "driver_diagnostics", kind = kind.id(), "ipc call");
    driver_diagnostics_impl(&state, kind)
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
        assert_eq!(sanitize_file_name(&"x".repeat(500)).len(), 200);
        // Truncation is by bytes, never inside a character, and keeps the extension.
        let long = format!("{}.csv", "日".repeat(100));
        let cut = sanitize_file_name(&long);
        assert!(cut.len() <= 200 && cut.ends_with(".csv"), "{cut}");
        assert!(cut.starts_with('日') && std::str::from_utf8(cut.as_bytes()).is_ok());
        let emoji = "😀".repeat(100);
        assert!(sanitize_file_name(&emoji).len() <= 200);
        // Windows device names get a suffix, also with extensions and in any case.
        assert_eq!(sanitize_file_name("CON"), "CON_");
        assert_eq!(sanitize_file_name("nul.txt"), "nul_.txt");
        assert_eq!(sanitize_file_name("Com1.tar.gz"), "Com1_.tar.gz");
        assert_eq!(sanitize_file_name("LPT9"), "LPT9_");
        assert_eq!(sanitize_file_name("COM0"), "COM0");
        assert_eq!(sanitize_file_name("console.txt"), "console.txt");
        assert_eq!(sanitize_file_name("aux"), "aux_");
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
        // A target name near the limit still works (the temp file name is short).
        let long = dir.path().join(sanitize_file_name(&"y".repeat(500)));
        write_atomic(&long, b"x").unwrap();
        assert_eq!(std::fs::read(&long).unwrap(), b"x");
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
    /// Oracle: the report explains the client state, mentions the configured folder and never
    /// panics, whether or not Instant Client exists on the machine that runs the tests.
    #[cfg(feature = "oracle")]
    #[tokio::test]
    async fn oracle_diagnostics_describe_the_client() {
        let (state, _) = crate::state::testing::test_state(crate::state::build_registry());
        let list = list_drivers_impl(&state);
        let listed = list
            .iter()
            .find(|d| d.kind == DbKind::Oracle)
            .expect("oracle is listed");
        assert!(matches!(
            listed.status,
            DriverStatus::Available | DriverStatus::Unavailable { .. }
        ));
        state
            .connections
            .store()
            .set_oracle_client(&dbstudio_store::OracleClientSettings {
                lib_dir: Some("/nonexistent/instantclient".into()),
            })
            .unwrap();
        let d = driver_diagnostics_impl(&state, DbKind::Oracle).unwrap();
        match d.status {
            DriverStatus::Available => {
                assert!(d.message.contains("loaded"), "{}", d.message);
                assert!(d.help_url.is_none());
            }
            DriverStatus::Unavailable { reason } => {
                assert!(
                    matches!(
                        reason,
                        UnavailableReason::NativeClientMissing
                            | UnavailableReason::NativeClientTooOld
                    ),
                    "{reason:?}"
                );
                assert!(
                    d.message.contains("/nonexistent/instantclient"),
                    "{}",
                    d.message
                );
                assert!(d.message.contains("Locations tried"), "{}", d.message);
                assert!(d.message.contains("Instant Client"), "{}", d.message);
                assert!(
                    d.help_url
                        .as_deref()
                        .is_some_and(|u| u.starts_with("https://www.oracle.com/"))
                );
            }
            other => panic!("unexpected status {other:?}"),
        }
        // Same answer as `list_drivers` after the probe.
        let again = list_drivers_impl(&state);
        assert_eq!(
            again
                .iter()
                .find(|d| d.kind == DbKind::Oracle)
                .unwrap()
                .status,
            d.status
        );
        dbstudio_driver_oracle::configure_client(None);
    }
}
