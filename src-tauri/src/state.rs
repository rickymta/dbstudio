#[cfg(any(feature = "postgres", feature = "mysql"))]
use std::sync::Arc as DriverArc;
use std::sync::Arc;

use dbstudio_core::DriverRegistry;
use dbstudio_store::ConnectionService;

use crate::host::Host;
use crate::session::SessionManager;

/// Shared application state managed by Tauri.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Registered database drivers (those enabled by cargo features).
    pub registry: Arc<DriverRegistry>,
    /// Saved profiles (SQLite) and their passwords (OS keychain).
    pub connections: Arc<ConnectionService>,
    /// Open database sessions.
    pub sessions: Arc<SessionManager>,
    /// Desktop-shell services (paths, dialogs).
    pub host: Arc<dyn Host>,
}

impl AppState {
    /// Assemble the state from a driver registry and the connection service.
    pub fn new(
        registry: DriverRegistry,
        connections: ConnectionService,
        host: Arc<dyn Host>,
    ) -> Self {
        let registry = Arc::new(registry);
        AppState {
            sessions: Arc::new(SessionManager::new(Arc::clone(&registry))),
            registry,
            connections: Arc::new(connections),
            host,
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
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use dbstudio_core::Result;
    use dbstudio_store::{MemorySecretStore, Store};

    use super::*;

    /// [`Host`] that records calls instead of showing dialogs or opening windows.
    #[derive(Debug, Default)]
    pub struct FakeHost {
        pub log_dir: PathBuf,
        /// Path the fake "Save" dialog returns (`None` = user cancelled).
        pub save_to: Mutex<Option<PathBuf>>,
        pub suggested: Mutex<Vec<String>>,
        pub opened: Mutex<Vec<PathBuf>>,
    }

    impl Host for FakeHost {
        fn app_identity(&self) -> (String, String) {
            ("DBStudio".into(), "0.0.0-test".into())
        }
        fn log_dir(&self) -> Result<PathBuf> {
            Ok(self.log_dir.clone())
        }
        fn open_dir(&self, dir: &Path) -> Result<()> {
            self.opened.lock().unwrap().push(dir.to_path_buf());
            Ok(())
        }
        fn pick_save_path(&self, suggested_name: &str) -> Result<Option<PathBuf>> {
            self.suggested
                .lock()
                .unwrap()
                .push(suggested_name.to_string());
            Ok(self.save_to.lock().unwrap().clone())
        }
    }

    /// State with an in-memory database and secret store (never touches the real keychain).
    pub fn test_state(registry: DriverRegistry) -> (AppState, Arc<MemorySecretStore>) {
        let (state, secrets, _) = test_state_with_host(registry);
        (state, secrets)
    }

    /// Like [`test_state`], also returning the fake host.
    pub fn test_state_with_host(
        registry: DriverRegistry,
    ) -> (AppState, Arc<MemorySecretStore>, Arc<FakeHost>) {
        let secrets = Arc::new(MemorySecretStore::new());
        let service = ConnectionService::new(Store::open_in_memory().unwrap(), secrets.clone());
        let host = Arc::new(FakeHost::default());
        (
            AppState::new(registry, service, host.clone()),
            secrets,
            host,
        )
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
