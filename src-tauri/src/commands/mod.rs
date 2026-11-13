//! IPC commands exposed to the frontend. Keep them thin: each `#[tauri::command]` only unpacks
//! Tauri types and calls a plain function (`*_impl`) that takes `&AppState`, so the logic is
//! testable without a Tauri runtime.

use dbstudio_core::{Error, Result};

pub mod app;
pub mod browse;
pub mod connections;
pub mod sessions;

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
