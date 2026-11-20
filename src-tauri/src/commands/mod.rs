//! IPC commands exposed to the frontend. Keep them thin: each `#[tauri::command]` only unpacks
//! Tauri types and calls a plain function (`*_impl`) that takes `&AppState`, so the logic is
//! testable without a Tauri runtime.

use dbstudio_core::{Error, Result};

pub mod app;
pub mod background;
pub mod backup;
pub mod browse;
pub mod changes;
pub mod connections;
pub mod debug;
pub mod editor;
pub mod erd;
pub mod export;
pub mod files;
pub mod history;
pub mod jobs;
pub mod keyvalue;
pub mod plan;
pub mod sessions;
pub mod ssh;
pub mod support;
pub mod uri;

/// Run blocking work (SQLite, keychain) off the async executor threads.
pub(crate) async fn blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Other(format!("background task failed: {e}")))?
}
