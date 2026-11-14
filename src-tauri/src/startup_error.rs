//! Startup failures: classify the error, show a native blocking dialog and exit cleanly.
//!
//! Returning `Err` from the Tauri `setup` hook makes Tauri panic inside the event loop, which
//! aborts the process (SIGABRT) and makes macOS show a crash report. Instead `setup` calls
//! [`report`], returns `Ok` and the process exits with code 1 once the dialog is closed.

use std::path::{Path, PathBuf};

use crate::notify::Lang;

/// Environment variable overriding the data directory (database, locks, ...).
pub const DATA_DIR_ENV: &str = "DBSTUDIO_DATA_DIR";
/// Test only: print the message to stderr and exit instead of showing the dialog.
pub const NO_DIALOG_ENV: &str = "DBSTUDIO_STARTUP_NO_DIALOG";

/// Marker of the store's "schema too new" error (see `dbstudio-store` migrations).
const NEWER_SCHEMA_MARKER: &str = "is newer than this application supports";

/// Kind of startup failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The data was written by a newer DBStudio.
    NewerSchema,
    /// Anything else.
    Other,
}

/// Classify an error message.
pub fn classify(message: &str) -> Kind {
    if message.contains(NEWER_SCHEMA_MARKER) {
        Kind::NewerSchema
    } else {
        Kind::Other
    }
}

/// Data directory: `DBSTUDIO_DATA_DIR` when set and non-empty, otherwise `default`.
pub fn resolve_data_dir(env: Option<&str>, default: PathBuf) -> PathBuf {
    match env {
        Some(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => default,
    }
}

/// Whether `DBSTUDIO_DATA_DIR` is set to a non-empty value.
pub fn has_custom_data_dir() -> bool {
    std::env::var(DATA_DIR_ENV).is_ok_and(|v| !v.trim().is_empty())
}

/// Title and body of the dialog.
pub fn message(
    lang: Lang,
    error: &str,
    data_dir: Option<&Path>,
    log_dir: Option<&Path>,
) -> (String, String) {
    let dir = |p: Option<&Path>| p.map(|p| p.display().to_string()).unwrap_or_default();
    let (title, body) = match (classify(error), lang) {
        (Kind::NewerSchema, Lang::Vi) => (
            "DBStudio không thể khởi động",
            format!(
                "Dữ liệu được tạo bởi phiên bản DBStudio mới hơn. Hãy cập nhật ứng dụng.\n\nThư mục dữ liệu: {}",
                dir(data_dir)
            ),
        ),
        (Kind::NewerSchema, Lang::En) => (
            "DBStudio cannot start",
            format!(
                "The data was created by a newer version of DBStudio. Please update the application.\n\nData folder: {}",
                dir(data_dir)
            ),
        ),
        (Kind::Other, Lang::Vi) => (
            "DBStudio không thể khởi động",
            format!("{error}\n\nXem nhật ký tại: {}", dir(log_dir)),
        ),
        (Kind::Other, Lang::En) => (
            "DBStudio cannot start",
            format!("{error}\n\nSee the log in: {}", dir(log_dir)),
        ),
    };
    (title.to_string(), body)
}

/// Show the error, then exit the process with code 1. Returns immediately (the dialog runs on
/// its own thread because the event loop on this one must keep running for it to appear).
pub fn report(app: &tauri::AppHandle, error: &str, data_dir: Option<&Path>) {
    use tauri::Manager;
    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

    let log_dir = app.path().app_log_dir().ok();
    let lang = dbstudio_backup::notify::language_for(dbstudio_store::UiLanguage::System);
    let (title, body) = message(lang, error, data_dir, log_dir.as_deref());
    if std::env::var_os(NO_DIALOG_ENV).is_some() {
        eprintln!("{title}: {body}");
        std::process::exit(1);
    }
    for window in app.webview_windows().values() {
        let _ = window.hide();
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        handle
            .dialog()
            .message(body)
            .title(title)
            .kind(MessageDialogKind::Error)
            .blocking_show();
        std::process::exit(1);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_newer_schema() {
        let e = "cannot open x: database schema version 8 is newer than this application supports (7); please update DBStudio";
        assert_eq!(classify(e), Kind::NewerSchema);
        assert_eq!(classify("permission denied"), Kind::Other);
    }

    #[test]
    fn data_dir_env_overrides() {
        let d = PathBuf::from("/default");
        assert_eq!(resolve_data_dir(None, d.clone()), d);
        assert_eq!(resolve_data_dir(Some("  "), d.clone()), d);
        assert_eq!(resolve_data_dir(Some("/x/y"), d), PathBuf::from("/x/y"));
    }

    #[test]
    fn messages_per_kind_and_language() {
        let data = Path::new("/data");
        let log = Path::new("/log");
        let (_, b) = message(
            Lang::Vi,
            "x is newer than this application supports",
            Some(data),
            Some(log),
        );
        assert!(b.contains("mới hơn") && b.contains("/data") && !b.contains("/log"));
        let (_, b) = message(Lang::En, "disk full", Some(data), Some(log));
        assert!(b.contains("disk full") && b.contains("/log"));
    }
}
