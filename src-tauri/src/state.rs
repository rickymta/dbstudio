#[cfg(any(feature = "postgres", feature = "mysql"))]
use std::sync::Arc as DriverArc;
use std::sync::Arc;

use dbstudio_core::DriverRegistry;
use dbstudio_store::ConnectionService;

use crate::session::SessionManager;

/// Shared application state managed by Tauri.
#[derive(Debug)]
pub struct AppState {
    /// Registered database drivers (those enabled by cargo features).
    pub registry: Arc<DriverRegistry>,
    /// Saved profiles (SQLite) and their passwords (OS keychain).
    pub connections: Arc<ConnectionService>,
    /// Open database sessions.
    pub sessions: SessionManager,
}

impl AppState {
    /// Assemble the state from a driver registry and the connection service.
    pub fn new(registry: DriverRegistry, connections: ConnectionService) -> Self {
        let registry = Arc::new(registry);
        AppState {
            sessions: SessionManager::new(Arc::clone(&registry)),
            registry,
            connections: Arc::new(connections),
        }
    }
}

/// Registry containing the drivers enabled by cargo features.
pub fn build_registry() -> DriverRegistry {
    #[allow(unused_mut)]
    let mut registry = DriverRegistry::new();
    #[cfg(feature = "postgres")]
    registry.register(DriverArc::new(
        dbstudio_driver_postgres::PostgresDriver::new(),
    ));
    #[cfg(feature = "mysql")]
    registry.register(DriverArc::new(dbstudio_driver_mysql::MySqlDriver::new()));
    registry
}

#[cfg(test)]
pub(crate) mod testing {
    use dbstudio_store::{MemorySecretStore, Store};

    use super::*;

    /// State with an in-memory database and secret store (never touches the real keychain).
    pub fn test_state(registry: DriverRegistry) -> (AppState, Arc<MemorySecretStore>) {
        let secrets = Arc::new(MemorySecretStore::new());
        let service = ConnectionService::new(Store::open_in_memory().unwrap(), secrets.clone());
        (AppState::new(registry, service), secrets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{DbKind, DriverStatus};

    #[test]
    fn registry_follows_features() {
        let (state, _) = testing::test_state(build_registry());
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
