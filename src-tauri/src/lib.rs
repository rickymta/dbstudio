//! DBStudio desktop shell. Wires `dbstudio-core` and `dbstudio-store` into Tauri; business logic
//! lives in those crates and in the plain functions behind each command.

#[cfg(all(feature = "dev-bridge", not(debug_assertions)))]
compile_error!(
    "the `dev-bridge` feature is for debug builds only and must not be enabled in release"
);

mod background;
#[cfg(all(feature = "dev-bridge", debug_assertions))]
// With `custom-protocol` (also enabled by `--all-features`) the bridge is compiled but never started.
#[cfg_attr(feature = "custom-protocol", allow(dead_code))]
mod bridge;
mod commands;
mod events;
mod grants;
mod guard;
mod history;
mod host;
mod jobs;
mod lease;
mod logging;
mod redact;
mod scheduler;
mod session;
mod sink;
mod state;
mod tray;

use std::sync::Arc;

use dbstudio_store::{ConnectionService, KeyringSecretStore, Store};
use tauri::{Manager, RunEvent};

/// File name of the SQLite database inside the app data directory.
const DATABASE_FILE: &str = "dbstudio.sqlite";

/// Open the profile database and wire the keychain; called once from `setup`.
fn build_state(app: &tauri::App) -> Result<state::AppState, Box<dyn std::error::Error>> {
    let host = Arc::new(host::TauriHost(app.handle().clone()));
    let data_dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    let db_path = data_dir.join(DATABASE_FILE);
    let store =
        Store::open(&db_path).map_err(|e| format!("cannot open {}: {e}", db_path.display()))?;
    let secrets = Arc::new(KeyringSecretStore::new());
    let connections = ConnectionService::new(store, secrets.clone());
    tracing::info!(database = %db_path.display(), "profile database ready");
    let settings = connections.settings().unwrap_or_default();
    if logging::set_level(settings.log_level) {
        tracing::info!(level = ?settings.log_level, "log level applied from settings");
    }
    // The Oracle driver loads Instant Client lazily from the `oracle_client` setting: tell it
    // where to look before the registry (and its first probe) exists.
    #[cfg(feature = "oracle")]
    if let Ok(client) = connections.store().get_oracle_client() {
        dbstudio_driver_oracle::configure_client(client.lib_dir.as_deref());
    }
    let state = state::AppState::new(state::build_registry(), connections, secrets, host)
        .with_data_dir(data_dir);
    state.events.attach_tauri(app.handle().clone());
    Ok(state)
}

/// Build and run the Tauri application.
pub fn run() {
    let app = tauri::Builder::default()
        // Must be registered first: a second launch only brings the running window forward.
        .plugin(tauri_plugin_single_instance::init(
            background::on_second_instance,
        ))
        .on_window_event(background::on_window_event)
        .plugin(tauri_plugin_dialog::init())
        // The opener plugin is only used from Rust; no capability grants it to the webview.
        .plugin(tauri_plugin_opener::init())
        // A page (re)load discards the editor tabs that held the leases: close them so no
        // connection keeps a transaction open for a tab that no longer exists.
        .on_page_load(|webview, payload| {
            if matches!(payload.event(), tauri::webview::PageLoadEvent::Started) {
                if let Some(state) = webview.try_state::<state::AppState>() {
                    let state = state.inner().clone();
                    tauri::async_runtime::spawn(async move {
                        state.leases.close_all().await;
                    });
                }
            }
        })
        // The notification and autostart plugins are only used from Rust (see `background`);
        // registering them is side-effect free: nothing is shown and autostart stays off.
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            // Launched at login the app starts hidden in the tray.
            Some(vec![background::BACKGROUND_ARG]),
        ))
        .setup(|app| {
            let log_dir = app.path().app_log_dir().ok();
            logging::init(log_dir.as_deref(), cfg!(debug_assertions));
            tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting DBStudio");
            let state =
                build_state(app).inspect_err(|e| tracing::error!(error = %e, "startup failed"))?;
            #[cfg(all(
                feature = "dev-bridge",
                debug_assertions,
                not(feature = "custom-protocol")
            ))]
            bridge::spawn_from_env(state.clone());
            background::setup(app.handle(), &state);
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::app::app_info,
            commands::app::list_drivers,
            commands::app::log_dir,
            commands::app::open_log_dir,
            commands::app::settings_get,
            commands::app::settings_set,
            commands::app::save_cell_value,
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
            commands::sessions::object_search,
            commands::sessions::query_cancel,
            commands::browse::table_details,
            commands::browse::table_fetch,
            commands::browse::table_count,
            // Phase 2-8
            commands::app::driver_diagnostics,
            commands::ssh::ssh_trust_host,
            commands::uri::connection_parse_uri,
            commands::editor::script_analyze,
            commands::editor::script_vocabulary,
            commands::editor::query_execute,
            commands::editor::lease_open,
            commands::editor::lease_close,
            commands::editor::lease_execute,
            commands::editor::lease_fetch_more,
            commands::editor::lease_discard_result,
            commands::editor::lease_cancel,
            commands::editor::lease_begin,
            commands::editor::lease_commit,
            commands::editor::lease_rollback,
            commands::editor::lease_set_auto_commit,
            commands::editor::lease_set_context,
            commands::editor::lease_state,
            commands::editor::sql_file_open,
            commands::editor::sql_file_save,
            commands::history::history_list,
            commands::history::history_delete,
            commands::history::history_clear,
            commands::history::saved_queries_list,
            commands::history::saved_query_save,
            commands::history::saved_query_delete,
            commands::history::saved_folder_save,
            commands::history::saved_folder_delete,
            commands::history::editor_tabs_get,
            commands::history::editor_tabs_set,
            commands::changes::table_editability,
            commands::changes::changes_preview,
            commands::changes::changes_apply,
            commands::plan::plan_explain,
            commands::plan::plan_parse,
            commands::plan::plan_file_save,
            commands::plan::plan_file_open,
            commands::erd::schema_model,
            commands::erd::erd_layouts_list,
            commands::erd::erd_layout_get,
            commands::erd::erd_layout_save,
            commands::erd::erd_layout_delete,
            commands::erd::ddl_generate,
            commands::files::file_pick_save,
            commands::files::file_pick_open,
            commands::files::file_save_bytes,
            commands::export::export_start,
            commands::export::export_presets_list,
            commands::export::export_preset_save,
            commands::export::export_preset_delete,
            commands::export::import_preview,
            commands::export::import_start,
            commands::jobs::jobs_list,
            commands::jobs::job_get,
            commands::jobs::job_cancel,
            commands::jobs::jobs_clear_finished,
            commands::backup::backup_jobs_list,
            commands::backup::backup_job_save,
            commands::backup::backup_job_delete,
            commands::backup::backup_job_run_now,
            commands::backup::backup_run_manual,
            commands::backup::backup_runs_list,
            commands::backup::backup_inspect,
            commands::backup::backup_verify,
            commands::backup::restore_start,
            commands::backup::backup_tools_detect,
            commands::backup::backup_tool_set_path,
            commands::backup::schedule_preview,
            commands::background::background_get,
            commands::background::background_set,
            commands::keyvalue::object_children_page,
            commands::keyvalue::key_read,
            commands::keyvalue::keys_delete_pattern,
        ])
        .build(tauri::generate_context!())
        .expect("error while building DBStudio");

    app.run(|handle, event| {
        if matches!(event, RunEvent::Exit) {
            if let Some(state) = handle.try_state::<state::AppState>() {
                // Leases first (they roll back open transactions), then the sessions.
                tauri::async_runtime::block_on(async {
                    state.leases.close_all().await;
                    state.sessions.close_all().await;
                });
            }
            tracing::info!("DBStudio exited");
            logging::shutdown();
        }
    });
}

#[cfg(all(test, any(feature = "postgres", feature = "mysql")))]
mod it;
#[cfg(test)]
mod leak_test;
#[cfg(test)]
mod scaffold_tests;
#[cfg(test)]
mod tests;
