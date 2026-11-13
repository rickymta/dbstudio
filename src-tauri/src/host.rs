//! Services that need the desktop shell (paths, native dialogs, file manager), behind a trait so
//! command logic stays testable and the dev bridge can reuse it.

use std::path::{Path, PathBuf};

use dbstudio_core::{Error, Result};
use serde::{Deserialize, Serialize};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

/// File type filter of a native dialog, e.g. `{"name": "CSV", "extensions": ["csv"]}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFilter {
    /// Label shown in the dialog.
    pub name: String,
    /// Extensions without the dot.
    pub extensions: Vec<String>,
}

/// Desktop-shell operations used by commands.
pub trait Host: Send + Sync + std::fmt::Debug {
    /// Application name and version.
    fn app_identity(&self) -> (String, String);
    /// Directory holding the log files.
    fn log_dir(&self) -> Result<PathBuf>;
    /// Show a directory in the OS file manager.
    fn open_dir(&self, dir: &Path) -> Result<()>;
    /// Ask the user (native Save dialog) where to save; `None` when cancelled. Blocks until the
    /// dialog is closed, so call it from a blocking thread.
    fn pick_save_path(&self, suggested_name: &str) -> Result<Option<PathBuf>>;
    /// Native Open dialog for a file (matching `filters`) or a directory; `None` when cancelled.
    /// Blocks like [`Host::pick_save_path`].
    fn pick_open_path(&self, filters: &[FileFilter], directory: bool) -> Result<Option<PathBuf>>;
    /// Native Save dialog restricted to `filters`; `None` when cancelled. Blocks.
    fn pick_save_path_filtered(
        &self,
        suggested_name: &str,
        filters: &[FileFilter],
    ) -> Result<Option<PathBuf>>;
    /// Show an OS notification (used by the backup package for finished runs).
    #[allow(dead_code)]
    fn notify(&self, title: &str, body: &str) -> Result<()>;
}

/// [`Host`] backed by a running Tauri app.
#[derive(Debug, Clone)]
pub struct TauriHost(pub tauri::AppHandle);

impl Host for TauriHost {
    fn app_identity(&self) -> (String, String) {
        let info = self.0.package_info();
        (info.name.clone(), info.version.to_string())
    }

    fn log_dir(&self) -> Result<PathBuf> {
        self.0
            .path()
            .app_log_dir()
            .map_err(|e| Error::Other(format!("log directory unavailable: {e}")))
    }

    fn open_dir(&self, dir: &Path) -> Result<()> {
        self.0
            .opener()
            .open_path(dir.to_string_lossy(), None::<&str>)
            .map_err(|e| Error::Other(format!("cannot open {}: {e}", dir.display())))
    }

    fn pick_save_path(&self, suggested_name: &str) -> Result<Option<PathBuf>> {
        self.pick_save_path_filtered(suggested_name, &[])
    }

    fn pick_open_path(&self, filters: &[FileFilter], directory: bool) -> Result<Option<PathBuf>> {
        let mut dialog = self.0.dialog().file();
        for f in filters {
            let exts: Vec<&str> = f.extensions.iter().map(String::as_str).collect();
            dialog = dialog.add_filter(&f.name, &exts);
        }
        let picked = if directory {
            dialog.blocking_pick_folder()
        } else {
            dialog.blocking_pick_file()
        };
        into_path(picked, "open")
    }

    fn pick_save_path_filtered(
        &self,
        suggested_name: &str,
        filters: &[FileFilter],
    ) -> Result<Option<PathBuf>> {
        let mut dialog = self.0.dialog().file().set_file_name(suggested_name);
        for f in filters {
            let exts: Vec<&str> = f.extensions.iter().map(String::as_str).collect();
            dialog = dialog.add_filter(&f.name, &exts);
        }
        into_path(dialog.blocking_save_file(), "save")
    }

    fn notify(&self, title: &str, body: &str) -> Result<()> {
        self.0
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()
            .map_err(|e| Error::Other(format!("cannot show notification: {e}")))
    }
}

fn into_path(picked: Option<tauri_plugin_dialog::FilePath>, what: &str) -> Result<Option<PathBuf>> {
    match picked {
        None => Ok(None),
        Some(p) => p
            .into_path()
            .map(Some)
            .map_err(|e| Error::Other(format!("unusable {what} path: {e}"))),
    }
}
