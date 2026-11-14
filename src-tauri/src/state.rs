#[cfg(any(feature = "postgres", feature = "mysql"))]
use std::sync::Arc as DriverArc;
use std::sync::Arc;

use dbstudio_core::DriverRegistry;
use dbstudio_store::{ConnectionService, SecretStore};

use crate::events::{EventEmitter, EventHub};
use crate::grants::PathGrants;
use crate::guard::ConfirmTokens;
use crate::host::Host;
use crate::jobs::JobManager;
use crate::lease::LeaseManager;
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
    /// The keychain (SSH secrets and other accounts besides the connection password).
    pub secrets: Arc<dyn SecretStore>,
    /// Application events (Tauri webview + dev bridge).
    pub events: Arc<EventHub>,
    /// Dedicated connections of editor / console tabs.
    pub leases: Arc<LeaseManager>,
    /// Background jobs.
    pub jobs: Arc<JobManager>,
    /// Pending confirmations for destructive operations.
    pub confirm: Arc<ConfirmTokens>,
    /// Paths the webview may make the backend read or write.
    pub grants: Arc<PathGrants>,
    /// Application data directory (database, per-job backup lock files shared with the CLI).
    pub data_dir: Arc<std::path::PathBuf>,
}

impl AppState {
    /// Assemble the state from a driver registry, the connection service and its keychain.
    pub fn new(
        registry: DriverRegistry,
        connections: ConnectionService,
        secrets: Arc<dyn SecretStore>,
        host: Arc<dyn Host>,
    ) -> Self {
        // History rows still `running` were left by a process that died mid-statement.
        if let Err(e) = connections.store().history_mark_interrupted() {
            tracing::warn!(error = %e, "cannot close stale history entries");
        }
        let registry = Arc::new(registry);
        let events = EventHub::new();
        let emitter: Arc<dyn EventEmitter> = events.clone();
        let leases = Arc::new(LeaseManager::new(Arc::clone(&emitter)));
        AppState {
            sessions: Arc::new(SessionManager::with_leases(
                Arc::clone(&registry),
                Arc::clone(&leases),
            )),
            registry,
            connections: Arc::new(connections),
            host,
            secrets,
            jobs: Arc::new(JobManager::new(emitter)),
            events,
            leases,
            confirm: Arc::new(ConfirmTokens::new()),
            grants: Arc::new(PathGrants::new()),
            data_dir: Arc::new(std::env::temp_dir().join("dbstudio")),
        }
    }

    /// The same state with the real application data directory.
    pub fn with_data_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.data_dir = Arc::new(dir);
        self
    }

    /// Directory of the per-job lock files, shared with `dbstudio-cli`.
    pub fn locks_dir(&self) -> std::path::PathBuf {
        self.data_dir.join("locks")
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
    // CockroachDB and YugabyteDB YSQL: flavours of the PostgreSQL driver (package N1).
    #[cfg(feature = "postgres")]
    dbstudio_driver_postgres::register_flavors(&mut registry);
    #[cfg(feature = "mysql")]
    registry.register(DriverArc::new(dbstudio_driver_mysql::MySqlDriver::new()));
    #[cfg(feature = "mssql")]
    dbstudio_driver_mssql::register(&mut registry);
    #[cfg(feature = "oracle")]
    dbstudio_driver_oracle::register(&mut registry);
    #[cfg(feature = "mongodb")]
    dbstudio_driver_mongodb::register(&mut registry);
    #[cfg(feature = "redis")]
    dbstudio_driver_redis::register(&mut registry);
    #[cfg(feature = "cassandra")]
    dbstudio_driver_cassandra::register(&mut registry);
    #[cfg(feature = "clickhouse")]
    dbstudio_driver_clickhouse::register(&mut registry);
    registry
}

#[cfg(test)]
pub(crate) mod testing {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use dbstudio_core::Result;
    use dbstudio_store::{MemorySecretStore, Store};

    use super::*;
    use crate::host::FileFilter;

    /// [`Host`] that records calls instead of showing dialogs or opening windows.
    #[derive(Debug, Default)]
    pub struct FakeHost {
        pub log_dir: PathBuf,
        /// Path the fake "Save" dialog returns (`None` = user cancelled).
        pub save_to: Mutex<Option<PathBuf>>,
        pub suggested: Mutex<Vec<String>>,
        pub opened: Mutex<Vec<PathBuf>>,
        /// Path the fake "Open" dialog returns (`None` = user cancelled).
        pub open_from: Mutex<Option<PathBuf>>,
        /// `(filters, directory)` of every Open dialog shown.
        pub open_requests: Mutex<Vec<(Vec<FileFilter>, bool)>>,
        /// Filters of every filtered Save dialog shown.
        pub save_filters: Mutex<Vec<Vec<FileFilter>>>,
        /// `(title, body)` of every notification.
        pub notifications: Mutex<Vec<(String, String)>>,
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
        fn pick_open_path(
            &self,
            filters: &[FileFilter],
            directory: bool,
        ) -> Result<Option<PathBuf>> {
            self.open_requests
                .lock()
                .unwrap()
                .push((filters.to_vec(), directory));
            Ok(self.open_from.lock().unwrap().clone())
        }
        fn pick_save_path_filtered(
            &self,
            suggested_name: &str,
            filters: &[FileFilter],
        ) -> Result<Option<PathBuf>> {
            self.save_filters.lock().unwrap().push(filters.to_vec());
            self.pick_save_path(suggested_name)
        }
        fn notify(&self, title: &str, body: &str) -> Result<()> {
            self.notifications
                .lock()
                .unwrap()
                .push((title.to_string(), body.to_string()));
            Ok(())
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
            AppState::new(registry, service, secrets.clone(), host.clone()),
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
        for (kind, enabled) in [
            (DbKind::SqlServer, cfg!(feature = "mssql")),
            (DbKind::Oracle, cfg!(feature = "oracle")),
            (DbKind::MongoDb, cfg!(feature = "mongodb")),
            (DbKind::Redis, cfg!(feature = "redis")),
        ] {
            assert_eq!(
                matches!(status(kind), DriverStatus::Planned { .. }),
                !enabled,
                "{kind:?}"
            );
        }
    }
}
