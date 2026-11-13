//! DBStudio desktop shell. Wires `dbstudio-core` into Tauri; no business logic lives here.

mod commands;
mod state;

use tracing_subscriber::EnvFilter;

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(if cfg!(debug_assertions) {
            "debug"
        } else {
            "info"
        })
    });
    // Ignore the error: a global subscriber may already be installed (e.g. in tests).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Build and run the Tauri application.
pub fn run() {
    init_tracing();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting DBStudio");

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state::AppState::default())
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
        .run(tauri::generate_context!())
        .expect("error while running DBStudio");
}
