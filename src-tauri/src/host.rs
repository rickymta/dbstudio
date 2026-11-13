//! Services that need the desktop shell (paths, native dialogs, file manager), behind a trait so
//! command logic stays testable and the dev bridge can reuse it.

use std::path::{Path, PathBuf};

use dbstudio_core::{Error, Result};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

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
        let picked = self
            .0
            .dialog()
            .file()
            .set_file_name(suggested_name)
            .blocking_save_file();
        match picked {
            None => Ok(None),
            Some(p) => p
                .into_path()
                .map(Some)
                .map_err(|e| Error::Other(format!("unusable save path: {e}"))),
        }
    }
}
