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
        .manage(state::AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::list_drivers
        ])
        .run(tauri::generate_context!())
        .expect("error while running DBStudio");
}
