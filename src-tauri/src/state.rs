#[cfg(any(feature = "postgres", feature = "mysql"))]
use std::sync::Arc;

use dbstudio_core::DriverRegistry;

/// Shared application state managed by Tauri.
#[derive(Debug)]
pub struct AppState {
    /// Registered database drivers (those enabled by cargo features).
    pub registry: DriverRegistry,
}

impl Default for AppState {
    fn default() -> Self {
        #[allow(unused_mut)]
        let mut registry = DriverRegistry::new();
        #[cfg(feature = "postgres")]
        registry.register(Arc::new(dbstudio_driver_postgres::PostgresDriver::new()));
        #[cfg(feature = "mysql")]
        registry.register(Arc::new(dbstudio_driver_mysql::MySqlDriver::new()));
        AppState { registry }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{DbKind, DriverStatus};

    #[test]
    fn registry_follows_features() {
        let state = AppState::default();
        let status = |kind| {
            state
                .registry
                .descriptors()
                .into_iter()
                .find(|d| d.kind == kind)
                .unwrap()
                .status
        };
        assert_eq!(
            status(DbKind::Postgres) == DriverStatus::Available,
            cfg!(feature = "postgres")
        );
        assert_eq!(
            status(DbKind::MySql) == DriverStatus::Available,
            cfg!(feature = "mysql")
        );
    }
}
