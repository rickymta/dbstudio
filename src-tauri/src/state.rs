use dbstudio_core::DriverRegistry;

/// Shared application state managed by Tauri.
#[derive(Debug, Default)]
pub struct AppState {
    /// Registered database drivers (none yet in phase 0).
    pub registry: DriverRegistry,
}
