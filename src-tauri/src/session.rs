//! Live database sessions. Pure Rust (no Tauri types) so it can be tested with mock drivers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use dbstudio_core::{
    ConnectionConfig, DriverDescriptor, DriverRegistry, Error, Result, ScriptDialect, Secret,
    ServerInfo, Session,
};
use serde::Serialize;
use uuid::Uuid;

use crate::events::NullEmitter;
use crate::lease::LeaseManager;

/// Connect timeout used when the profile does not specify one.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 15_000;

/// Returned by `session_open`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    /// Identifier of the new session.
    pub session_id: Uuid,
    /// Profile the session was opened from.
    pub connection_id: Uuid,
    /// Server product and version.
    pub server_info: ServerInfo,
    /// Descriptor (capabilities) of the driver in use.
    pub descriptor: DriverDescriptor,
    /// Script language of the session (editor lexer, splitter, parameters, classifier).
    pub dialect: ScriptDialect,
}

struct Entry {
    connection_id: Uuid,
    /// Profile settings at open time (read-only / environment for the guard).
    config: ConnectionConfig,
    session: Arc<dyn Session>,
}

/// What `connect` produced.
struct Connected {
    session: Arc<dyn Session>,
    server_info: ServerInfo,
    descriptor: DriverDescriptor,
    dialect: ScriptDialect,
}

/// Registry of open sessions. Several sessions may be open for the same connection.
pub struct SessionManager {
    registry: Arc<DriverRegistry>,
    leases: Arc<LeaseManager>,
    sessions: Mutex<HashMap<Uuid, Entry>>,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("open", &self.len())
            .finish()
    }
}

impl SessionManager {
    /// Create a manager that opens sessions through `registry` and has its own lease registry
    /// (no events are published).
    #[allow(dead_code)] // the application uses `with_leases`; tests use this one
    pub fn new(registry: Arc<DriverRegistry>) -> Self {
        Self::with_leases(registry, Arc::new(LeaseManager::new(Arc::new(NullEmitter))))
    }

    /// Create a manager that closes the leases of a session in `leases` before the session.
    pub fn with_leases(registry: Arc<DriverRegistry>, leases: Arc<LeaseManager>) -> Self {
        SessionManager {
            registry,
            leases,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Uuid, Entry>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Number of open sessions.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// True when no session is open.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Connect using the driver for `config.kind` (with timeout) and fetch the server info.
    /// The caller owns the returned session.
    async fn connect(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<Connected> {
        let driver = self.registry.get(config.kind).ok_or_else(|| {
            Error::Unsupported(format!("driver for {}", config.kind.display_name()))
        })?;
        let descriptor = driver.descriptor();
        let limit = Duration::from_millis(
            config
                .connect_timeout_ms
                .filter(|ms| *ms > 0)
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS),
        );
        let attempt = async {
            let session: Arc<dyn Session> = Arc::from(driver.connect(config, secret).await?);
            match session.server_info().await {
                Ok(info) => Ok((session, info)),
                Err(e) => {
                    let _ = session.close().await;
                    Err(e)
                }
            }
        };
        let (session, info) = tokio::time::timeout(limit, attempt)
            .await
            .map_err(|_| Error::Timeout)??;
        let dialect = session.script_dialect();
        Ok(Connected {
            session,
            server_info: info,
            descriptor,
            dialect,
        })
    }

    /// Open a new session for `config`.
    pub async fn open(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<SessionInfo> {
        let Connected {
            session,
            server_info,
            descriptor,
            dialect,
        } = self.connect(config, secret).await?;
        let session_id = Uuid::new_v4();
        self.lock().insert(
            session_id,
            Entry {
                connection_id: config.id,
                config: config.clone(),
                session,
            },
        );
        tracing::info!(
            session = %session_id,
            connection = %config.id,
            kind = config.kind.id(),
            host = %config.host,
            user = config.username.as_deref().unwrap_or(""),
            "session opened"
        );
        Ok(SessionInfo {
            session_id,
            connection_id: config.id,
            server_info,
            descriptor,
            dialect,
        })
    }

    /// Connect, read the server info and disconnect again (used by "Test connection").
    pub async fn test(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<ServerInfo> {
        let Connected {
            session,
            server_info: info,
            ..
        } = self.connect(config, secret).await?;
        if let Err(e) = session.close().await {
            tracing::debug!(error = %e, "closing test session failed");
        }
        tracing::info!(
            kind = config.kind.id(),
            host = %config.host,
            user = config.username.as_deref().unwrap_or(""),
            "connection test succeeded"
        );
        Ok(info)
    }

    /// The open session, or `NotFound`.
    pub fn get(&self, session_id: Uuid) -> Result<Arc<dyn Session>> {
        self.lock()
            .get(&session_id)
            .map(|e| Arc::clone(&e.session))
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))
    }

    /// The open session together with the profile settings it was opened with.
    pub fn get_with_config(
        &self,
        session_id: Uuid,
    ) -> Result<(Arc<dyn Session>, ConnectionConfig)> {
        self.lock()
            .get(&session_id)
            .map(|e| (Arc::clone(&e.session), e.config.clone()))
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))
    }

    /// Close a session (its leases first). Closing an unknown (already closed) session is not an error.
    pub async fn close(&self, session_id: Uuid) -> Result<()> {
        let entry = self.lock().remove(&session_id);
        match entry {
            Some(entry) => {
                self.leases.close_all_for_session(session_id).await;
                Self::shut_down(session_id, entry).await
            }
            None => Ok(()),
        }
    }

    /// Close every session opened from `connection_id`; returns how many were closed.
    pub async fn close_connection(&self, connection_id: Uuid) -> usize {
        let doomed: Vec<(Uuid, Entry)> = {
            let mut map = self.lock();
            let ids: Vec<Uuid> = map
                .iter()
                .filter(|(_, e)| e.connection_id == connection_id)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| map.remove(&id).map(|e| (id, e)))
                .collect()
        };
        let count = doomed.len();
        for (id, entry) in doomed {
            self.leases.close_all_for_session(id).await;
            let _ = Self::shut_down(id, entry).await;
        }
        count
    }

    /// Close every open session (application exit).
    pub async fn close_all(&self) {
        let all: Vec<(Uuid, Entry)> = self.lock().drain().collect();
        for (id, entry) in all {
            self.leases.close_all_for_session(id).await;
            let _ = Self::shut_down(id, entry).await;
        }
    }

    async fn shut_down(id: Uuid, entry: Entry) -> Result<()> {
        let result = entry.session.close().await;
        match &result {
            Ok(()) => {
                tracing::info!(session = %id, connection = %entry.connection_id, "session closed")
            }
            Err(e) => tracing::warn!(session = %id, error = %e, "session close failed"),
        }
        result
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Mock driver and session shared by the unit tests of this crate.

    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use dbstudio_core::{
        Capabilities, ColumnMeta, DbKind, Driver, EndOutcome, ExecSummary, ObjectNode, ObjectPath,
        QueryRequest, ResultSink, StatementEnd, TableCountRequest, TableDataRequest, Value,
        ValueKind,
    };

    use super::*;

    /// What the mock driver observed.
    #[derive(Debug, Default)]
    pub struct Probe {
        pub connects: AtomicUsize,
        pub closes: AtomicUsize,
        pub last_secret: Mutex<Option<Option<String>>>,
        pub cancels: Mutex<Vec<Uuid>>,
        /// Texts passed to `Session::execute`.
        pub executed: Mutex<Vec<String>>,
    }

    /// Driver whose sessions stream three row batches; it fails with `Authentication` when
    /// `require_password` is set and no secret is supplied.
    #[derive(Debug)]
    pub struct MockDriver {
        pub kind: DbKind,
        pub probe: Arc<Probe>,
        pub require_password: bool,
        pub connect_delay: Option<Duration>,
    }

    impl MockDriver {
        pub fn new(kind: DbKind) -> (Arc<MockDriver>, Arc<Probe>) {
            let probe = Arc::new(Probe::default());
            (
                Arc::new(MockDriver {
                    kind,
                    probe: probe.clone(),
                    require_password: false,
                    connect_delay: None,
                }),
                probe,
            )
        }
    }

    #[derive(Debug)]
    pub struct MockSession {
        probe: Arc<Probe>,
    }

    #[async_trait]
    impl Driver for MockDriver {
        fn descriptor(&self) -> DriverDescriptor {
            DriverDescriptor::new(
                self.kind,
                Capabilities::planned_for(self.kind),
                dbstudio_core::DriverStatus::Available,
            )
        }

        async fn connect(
            &self,
            _config: &ConnectionConfig,
            secret: Option<&Secret>,
        ) -> Result<Box<dyn Session>> {
            if let Some(d) = self.connect_delay {
                tokio::time::sleep(d).await;
            }
            *self.probe.last_secret.lock().unwrap() = Some(secret.map(|s| s.expose().to_string()));
            if self.require_password && secret.is_none() {
                return Err(Error::Authentication("password required".into()));
            }
            self.probe.connects.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(MockSession {
                probe: self.probe.clone(),
            }))
        }
    }

    #[async_trait]
    impl Session for MockSession {
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        async fn server_info(&self) -> Result<ServerInfo> {
            Ok(ServerInfo {
                product: "MockDB".into(),
                version: "1.2.3".into(),
                ..Default::default()
            })
        }
        async fn execute(
            &self,
            req: QueryRequest,
            sink: &mut dyn ResultSink,
        ) -> Result<ExecSummary> {
            self.probe.executed.lock().unwrap().push(req.text.clone());
            if req.text == "fail" {
                return Err(Error::Query {
                    message: "boom".into(),
                    position: Some(2),
                });
            }
            let len = u32::try_from(req.text.chars().count()).unwrap_or(u32::MAX);
            sink.on_statement_start(dbstudio_core::StatementStart {
                index: 0,
                range: dbstudio_core::TextRange { start: 0, end: len },
            })?;
            sink.on_columns(vec![ColumnMeta {
                name: "n".into(),
                type_name: "int8".into(),
                value_kind: ValueKind::Int,
                nullable: Some(false),
            }])?;
            sink.on_rows(vec![vec![Value::Int(1)]])?;
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Rows {
                    row_count: 1,
                    truncated: false,
                },
                elapsed_ms: 1,
            })?;
            Ok(ExecSummary {
                statements: 1,
                elapsed_ms: 1,
            })
        }
        async fn cancel(&self, query_id: Uuid) -> Result<()> {
            self.probe.cancels.lock().unwrap().push(query_id);
            Ok(())
        }
        async fn list_children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
            let _ = parent;
            Ok(vec![])
        }
        async fn object_ddl(&self, _path: &ObjectPath) -> Result<String> {
            Ok("CREATE TABLE t ();".into())
        }
        async fn fetch_table(
            &self,
            req: TableDataRequest,
            sink: &mut dyn ResultSink,
        ) -> Result<ExecSummary> {
            if req.raw_where.as_deref() == Some("slow") {
                // A query that never answers until it is cancelled or dropped.
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            sink.on_columns(vec![ColumnMeta {
                name: "id".into(),
                type_name: "int8".into(),
                value_kind: ValueKind::Int,
                nullable: Some(false),
            }])?;
            for chunk in [[1i64, 2], [3, 4], [5, 6]] {
                sink.on_rows(chunk.iter().map(|i| vec![Value::Int(*i)]).collect())?;
            }
            sink.on_message("done".into())?;
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Rows {
                    row_count: req.limit.min(6),
                    truncated: false,
                },
                elapsed_ms: 1,
            })?;
            Ok(ExecSummary {
                statements: 1,
                elapsed_ms: 1,
            })
        }
        async fn count_table(&self, _req: TableCountRequest) -> Result<u64> {
            Ok(42)
        }
        async fn close(&self) -> Result<()> {
            self.probe.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use dbstudio_core::DbKind;

    use super::testing::*;
    use super::*;

    fn config(kind: DbKind) -> ConnectionConfig {
        ConnectionConfig::new(kind, "t", "localhost")
    }

    fn manager(driver: Arc<MockDriver>) -> SessionManager {
        let mut registry = DriverRegistry::new();
        registry.register(driver);
        SessionManager::new(Arc::new(registry))
    }

    #[tokio::test]
    async fn open_get_close() {
        let (driver, probe) = MockDriver::new(DbKind::Postgres);
        let mgr = manager(driver);
        let cfg = config(DbKind::Postgres);
        let info = mgr.open(&cfg, None).await.unwrap();
        assert_eq!(info.connection_id, cfg.id);
        assert_eq!(info.server_info.product, "MockDB");
        assert_eq!(info.descriptor.kind, DbKind::Postgres);
        // The mock session keeps the default dialect.
        assert_eq!(info.dialect, dbstudio_core::ScriptDialect::Opaque);
        assert_eq!(
            serde_json::to_value(&info).unwrap()["dialect"],
            serde_json::json!({"name": "opaque"})
        );
        assert_eq!(mgr.len(), 1);
        assert!(mgr.get(info.session_id).is_ok());

        mgr.close(info.session_id).await.unwrap();
        assert!(mgr.is_empty());
        assert_eq!(probe.closes.load(Ordering::SeqCst), 1);
        assert!(matches!(mgr.get(info.session_id), Err(Error::NotFound(_))));
        // Closing again is harmless.
        mgr.close(info.session_id).await.unwrap();
        assert_eq!(probe.closes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn config_is_kept_per_session() {
        let (driver, _) = MockDriver::new(DbKind::Postgres);
        let mgr = manager(driver);
        let mut cfg = config(DbKind::Postgres);
        cfg.read_only = true;
        let info = mgr.open(&cfg, None).await.unwrap();
        let (_, kept) = mgr.get_with_config(info.session_id).unwrap();
        assert!(kept.read_only);
        assert_eq!(kept.id, cfg.id);
        assert!(matches!(
            mgr.get_with_config(Uuid::new_v4()),
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn closing_a_session_closes_its_leases_first() {
        use crate::lease::testing::FakeLease;
        let (driver, _) = MockDriver::new(DbKind::Postgres);
        let mut registry = DriverRegistry::new();
        registry.register(driver);
        let leases = Arc::new(LeaseManager::new(Arc::new(NullEmitter)));
        let mgr = SessionManager::with_leases(Arc::new(registry), leases.clone());
        let cfg = config(DbKind::Postgres);
        let a = mgr.open(&cfg, None).await.unwrap();
        let b = mgr.open(&cfg, None).await.unwrap();
        let closes = Arc::new(AtomicUsize::new(0));
        let mk = || -> Box<dyn dbstudio_core::Lease> {
            Box::new(FakeLease {
                closes: closes.clone(),
            })
        };
        leases.register(a.session_id, mk()).unwrap();
        leases.register(a.session_id, mk()).unwrap();
        let other = leases.register(b.session_id, mk()).unwrap();

        mgr.close(a.session_id).await.unwrap();
        assert_eq!(closes.load(Ordering::SeqCst), 2);
        assert_eq!(leases.count_for_session(a.session_id), 0);
        assert!(leases.get(other.lease_id).is_ok());

        // close_connection and close_all do the same.
        assert_eq!(mgr.close_connection(cfg.id).await, 1);
        assert_eq!(closes.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn unknown_session_is_not_found() {
        let (driver, _) = MockDriver::new(DbKind::Postgres);
        let mgr = manager(driver);
        assert!(matches!(mgr.get(Uuid::new_v4()), Err(Error::NotFound(_))));
    }

    #[tokio::test]
    async fn unregistered_driver_is_unsupported() {
        let (driver, probe) = MockDriver::new(DbKind::Postgres);
        let mgr = manager(driver);
        let err = mgr.open(&config(DbKind::Redis), None).await.unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)));
        assert!(mgr.test(&config(DbKind::Redis), None).await.is_err());
        assert_eq!(probe.connects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn several_sessions_per_connection_and_close_connection() {
        let (driver, probe) = MockDriver::new(DbKind::MySql);
        let mgr = manager(driver);
        let a = config(DbKind::MySql);
        let b = config(DbKind::MySql);
        let s1 = mgr.open(&a, None).await.unwrap();
        let s2 = mgr.open(&a, None).await.unwrap();
        let s3 = mgr.open(&b, None).await.unwrap();
        assert_ne!(s1.session_id, s2.session_id);
        assert_eq!(mgr.len(), 3);

        assert_eq!(mgr.close_connection(a.id).await, 2);
        assert!(mgr.get(s1.session_id).is_err() && mgr.get(s2.session_id).is_err());
        assert!(mgr.get(s3.session_id).is_ok());
        assert_eq!(probe.closes.load(Ordering::SeqCst), 2);

        mgr.close_all().await;
        assert!(mgr.is_empty());
        assert_eq!(probe.closes.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn connect_times_out() {
        let (mut driver, _) = MockDriver::new(DbKind::Postgres);
        Arc::get_mut(&mut driver).unwrap().connect_delay = Some(Duration::from_secs(30));
        let mgr = manager(driver);
        let mut cfg = config(DbKind::Postgres);
        cfg.connect_timeout_ms = Some(30);
        assert!(matches!(mgr.open(&cfg, None).await, Err(Error::Timeout)));
        assert!(mgr.is_empty());
    }

    #[tokio::test]
    async fn test_connects_and_closes_without_registering() {
        let (driver, probe) = MockDriver::new(DbKind::Postgres);
        let mgr = manager(driver);
        let info = mgr
            .test(&config(DbKind::Postgres), Some(&Secret::new("pw")))
            .await
            .unwrap();
        assert_eq!(info.version, "1.2.3");
        assert!(mgr.is_empty());
        assert_eq!(probe.closes.load(Ordering::SeqCst), 1);
        assert_eq!(
            probe.last_secret.lock().unwrap().clone(),
            Some(Some("pw".to_string()))
        );
    }
}
