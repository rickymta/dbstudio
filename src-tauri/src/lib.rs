//! DBStudio desktop shell. Wires `dbstudio-core` and `dbstudio-store` into Tauri; business logic
//! lives in those crates and in the plain functions behind each command.

mod commands;
mod logging;
mod session;
mod sink;
mod state;

use std::sync::Arc;

use dbstudio_store::{ConnectionService, KeyringSecretStore, Store};
use tauri::{Manager, RunEvent};

/// File name of the SQLite database inside the app data directory.
const DATABASE_FILE: &str = "dbstudio.sqlite";

/// Open the profile database and wire the keychain; called once from `setup`.
fn build_state(app: &tauri::App) -> Result<state::AppState, Box<dyn std::error::Error>> {
    let data_dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    let db_path = data_dir.join(DATABASE_FILE);
    let store =
        Store::open(&db_path).map_err(|e| format!("cannot open {}: {e}", db_path.display()))?;
    let connections = ConnectionService::new(store, Arc::new(KeyringSecretStore::new()));
    tracing::info!(database = %db_path.display(), "profile database ready");
    Ok(state::AppState::new(state::build_registry(), connections))
}

/// Build and run the Tauri application.
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let log_dir = app.path().app_log_dir().ok();
            logging::init(log_dir.as_deref());
            tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting DBStudio");
            let state =
                build_state(app).inspect_err(|e| tracing::error!(error = %e, "startup failed"))?;
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::app::app_info,
            commands::app::list_drivers,
            commands::connections::connections_list,
            commands::connections::connection_save,
            commands::connections::connection_delete,
            commands::connections::connection_duplicate,
            commands::connections::group_save,
            commands::connections::group_delete,
            commands::connections::connection_test,
            commands::connections::connections_export,
            commands::connections::connections_import,
            commands::sessions::session_open,
            commands::sessions::session_close,
            commands::sessions::object_children,
            commands::sessions::object_ddl,
            commands::sessions::query_cancel,
            commands::browse::table_details,
            commands::browse::table_fetch,
            commands::browse::table_count,
        ])
        .build(tauri::generate_context!())
        .expect("error while building DBStudio");

    app.run(|handle, event| {
        if matches!(event, RunEvent::Exit) {
            if let Some(state) = handle.try_state::<state::AppState>() {
                tauri::async_runtime::block_on(state.sessions.close_all());
            }
            tracing::info!("DBStudio exited");
        }
    });
}

#[cfg(test)]
mod tests;
