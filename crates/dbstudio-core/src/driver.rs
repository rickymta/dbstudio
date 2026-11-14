use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    ApplyResult, Capabilities, ChangeSet, ChildrenPage, ChildrenPageRequest, CollectingSink,
    ConnectionConfig, DbFamily, DbKind, DdlItem, DdlRequest, Editability, Error, ExecSummary,
    ExplainRequest, KeyReadRequest, KeyScanPage, KeyScanRequest, KeyValuePage, Lease, LeaseOptions,
    ObjectNode, ObjectPath, ObjectSearchRequest, QueryRequest, RawPlan, Result, ResultSink,
    SchemaModel, SchemaModelRequest, ScriptDialect, Secret, StatementResult, TableCountRequest,
    TableDataRequest, TableDetails,
};

/// Implementation status of a driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum DriverStatus {
    /// A driver is registered and usable.
    Available,
    /// Not implemented yet; scheduled for the given roadmap phase.
    Planned {
        /// Roadmap phase number.
        phase: u8,
    },
    /// Implemented but cannot run on this machine (e.g. Oracle Instant Client missing).
    Unavailable {
        /// Why. Details and instructions come from the `driver_diagnostics` command.
        reason: UnavailableReason,
    },
}

/// Why a registered driver cannot run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UnavailableReason {
    /// The vendor client library is not installed.
    NativeClientMissing,
    /// The vendor client library is older than the driver needs.
    NativeClientTooOld,
}

/// Static description of a driver, safe to send to the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverDescriptor {
    /// Database product.
    pub kind: DbKind,
    /// Product name for display.
    pub display_name: String,
    /// Default TCP port.
    pub default_port: u16,
    /// Data-model family.
    pub family: DbFamily,
    /// Supported features.
    pub capabilities: Capabilities,
    /// Whether the driver exists yet.
    pub status: DriverStatus,
}

impl DriverDescriptor {
    /// Descriptor with the standard metadata of `kind` and the given capabilities and status.
    pub fn new(kind: DbKind, capabilities: Capabilities, status: DriverStatus) -> Self {
        DriverDescriptor {
            kind,
            display_name: kind.display_name().to_string(),
            default_port: kind.default_port(),
            family: kind.family(),
            capabilities,
            status,
        }
    }

    /// Roadmap phase in which the driver of `kind` is planned.
    pub fn planned_phase(kind: DbKind) -> u8 {
        match kind {
            DbKind::Postgres | DbKind::MySql => 1,
            DbKind::SqlServer | DbKind::Oracle => 3,
            DbKind::MongoDb | DbKind::Redis => 4,
            _ => 10,
        }
    }

    /// Descriptor of a not-yet-implemented driver with its expected capabilities.
    pub fn planned(kind: DbKind) -> Self {
        Self::new(
            kind,
            Capabilities::planned_for(kind),
            DriverStatus::Planned {
                phase: Self::planned_phase(kind),
            },
        )
    }
}

/// Planned descriptors for every [`DbKind`], in stable order.
pub fn planned_descriptors() -> Vec<DriverDescriptor> {
    DbKind::ALL
        .into_iter()
        .map(DriverDescriptor::planned)
        .collect()
}

/// Basic information about a connected server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    /// Product name reported by the server.
    pub product: String,
    /// Version string.
    pub version: String,
    /// Further driver specific facts (edition, cluster name, ...).
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

/// A database product implementation.
#[async_trait]
pub trait Driver: Send + Sync {
    /// Static description including capabilities.
    fn descriptor(&self) -> DriverDescriptor;

    /// Open a connection. `secret` is the password or token, if any.
    async fn connect(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<Box<dyn Session>>;
}

/// One live session on a server.
///
/// A session represents the whole server: the driver manages its internal connections itself
/// (including connections to several databases of the same server), and all methods may be
/// called concurrently.
#[async_trait]
pub trait Session: Send + Sync {
    /// Check that the connection is alive.
    async fn ping(&self) -> Result<()>;

    /// Server product and version.
    async fn server_info(&self) -> Result<ServerInfo>;

    /// Execute the statements in `req`, streaming results into `sink` in batches of
    /// `req.batch_size` rows. If a sink method returns `Err`, stop and return that error.
    ///
    /// `Error::Query::position` is a zero-based character offset into the WHOLE text of
    /// `req.text` (not into the individual statement).
    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary>;

    /// Cancel the running query identified by `query_id`. Unsupported by default. For an id that
    /// is unknown or whose query already finished, implementations return `Ok(())`.
    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        let _ = query_id;
        Err(Error::Unsupported("cancel_query".into()))
    }

    /// List the children of `parent` in the object tree (`ObjectPath::root()` for the top level).
    async fn list_children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>>;

    /// DDL text (`CREATE ...`) that recreates the object at `path`. Unsupported by default.
    async fn object_ddl(&self, path: &ObjectPath) -> Result<String> {
        let _ = path;
        Err(Error::Unsupported("object_ddl".into()))
    }

    /// Columns, indexes, foreign keys and statistics of the table or view at `path`.
    /// Unsupported by default.
    async fn table_details(&self, path: &ObjectPath) -> Result<TableDetails> {
        let _ = path;
        Err(Error::Unsupported("table_details".into()))
    }

    /// Stream one page of table rows into `sink`: `on_columns`, any number of `on_rows`
    /// (batches of `req.batch_size`), then `on_statement_end`. Unsupported by default.
    ///
    /// Contract for implementors: the driver quotes every identifier and escapes every filter
    /// value according to its own dialect. Filter values are strings typed by the user; the
    /// database coerces them to the column type. `req.raw_where` is a `WHERE` clause body typed
    /// by the user and is appended verbatim (it is trusted, not escaped). The fetch can be
    /// cancelled with [`Session::cancel`] using `req.query_id`.
    async fn fetch_table(
        &self,
        req: TableDataRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let _ = (req, sink);
        Err(Error::Unsupported("fetch_table".into()))
    }

    /// Number of rows matching the filters of `req` (same quoting/escaping contract as
    /// [`Session::fetch_table`]). When `req.query_id` is set the count can be aborted with
    /// [`Session::cancel`]. Unsupported by default.
    async fn count_table(&self, req: TableCountRequest) -> Result<u64> {
        let _ = req;
        Err(Error::Unsupported("count_table".into()))
    }

    /// Search objects by name on the server.
    ///
    /// Contract: `req.query` is matched as a case-insensitive substring of the object name
    /// (`%`, `_` and other characters are literal); an empty or all-whitespace query returns an
    /// empty list. Results are [`ObjectNode`]s whose `path` follows the tree convention exactly
    /// (including `Folder` segments), so the UI can open them and locate them in the tree. They
    /// are ordered exact match, then prefix match, then substring match, each group by name then
    /// path, and truncated to `limit` (clamped to `1..=1000`). `detail` should carry short
    /// context such as `schema` or `database.schema`. With the root as scope a driver may
    /// restrict itself to the session's default database when searching across databases would
    /// need new connections (documented per driver). Unsupported by default.
    async fn search_objects(&self, req: ObjectSearchRequest) -> Result<Vec<ObjectNode>> {
        let _ = req;
        Err(Error::Unsupported("object_search".into()))
    }

    /// Language of the editor / console for this session (lexer, splitter, params, classifier).
    /// Defaults to [`ScriptDialect::Opaque`].
    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect::Opaque
    }

    /// Open a dedicated connection (see [`Lease`]). Unsupported by default.
    async fn open_lease(&self, opts: LeaseOptions) -> Result<Box<dyn Lease>> {
        let _ = opts;
        Err(Error::Unsupported("leases".into()))
    }

    /// One page of the children of a node. Default: [`Session::list_children`] + glob filter on
    /// the name + decimal-offset cursor.
    async fn list_children_page(&self, req: ChildrenPageRequest) -> Result<ChildrenPage> {
        crate::fallback::children_page(self, req).await
    }

    /// Whether the rows of a table can be edited and by which key. Default: derived from
    /// [`Session::table_details`].
    async fn editability(&self, path: &ObjectPath) -> Result<Editability> {
        crate::fallback::editability_from_details(self, path).await
    }

    /// The statements `apply_changes` would run, one per change, in application order.
    /// Unsupported by default.
    async fn preview_changes(&self, changes: &ChangeSet) -> Result<Vec<String>> {
        let _ = changes;
        Err(Error::Unsupported("data_editing".into()))
    }

    /// Apply a change set (all-or-nothing when [`Editability::atomic`]). Unsupported by default.
    async fn apply_changes(&self, changes: ChangeSet) -> Result<ApplyResult> {
        let _ = changes;
        Err(Error::Unsupported("data_editing".into()))
    }

    /// Explain one statement. Unsupported by default.
    async fn explain(&self, req: ExplainRequest) -> Result<RawPlan> {
        let _ = req;
        Err(Error::Unsupported("explain".into()))
    }

    /// Tables and relations of a scope. Default: built from `table_details` for at most 300
    /// tables, otherwise `Unsupported`.
    async fn schema_model(&self, req: SchemaModelRequest) -> Result<SchemaModel> {
        crate::fallback::schema_model_via_details(self, req).await
    }

    /// Ordered DDL of the objects in a scope. Unsupported by default.
    async fn generate_ddl(&self, req: DdlRequest) -> Result<Vec<DdlItem>> {
        let _ = req;
        Err(Error::Unsupported("ddl_generation".into()))
    }

    /// Read one page of a Redis key. Unsupported by default.
    async fn read_key(&self, req: KeyReadRequest) -> Result<KeyValuePage> {
        let _ = req;
        Err(Error::Unsupported("key_value".into()))
    }

    /// Capabilities of THIS server when they differ from the driver descriptor (flavour detected
    /// from `version()`, server version gates). `session_open` puts them in
    /// `SessionInfo.descriptor.capabilities`. `None` = same as the descriptor.
    fn capabilities(&self) -> Option<Capabilities> {
        None
    }

    /// One page of raw key names of a key-value database (export, backup). Names use the `Key`
    /// segment encoding ([`crate::encode_key_name`]). Unsupported by default.
    async fn scan_keys(&self, req: KeyScanRequest) -> Result<KeyScanPage> {
        let _ = req;
        Err(Error::Unsupported("scan_keys".into()))
    }

    /// Close the connection and release resources.
    async fn close(&self) -> Result<()>;
}

/// Run `req` on `session` and collect every statement result in memory. Intended for tests and
/// small results; prefer streaming into a custom [`ResultSink`] for large ones.
pub async fn execute_collect(
    session: &dyn Session,
    req: QueryRequest,
) -> Result<Vec<StatementResult>> {
    let mut sink = CollectingSink::new();
    session.execute(req, &mut sink).await?;
    Ok(sink.into_results())
}

/// Registry of available drivers keyed by [`DbKind`].
#[derive(Default)]
pub struct DriverRegistry {
    drivers: BTreeMap<DbKind, Arc<dyn Driver>>,
}

impl std::fmt::Debug for DriverRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverRegistry")
            .field("kinds", &self.drivers.keys())
            .finish()
    }
}

impl DriverRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) the driver for its kind.
    pub fn register(&mut self, driver: Arc<dyn Driver>) {
        self.drivers.insert(driver.descriptor().kind, driver);
    }

    /// Driver for `kind`, if registered.
    pub fn get(&self, kind: DbKind) -> Option<Arc<dyn Driver>> {
        self.drivers.get(&kind).cloned()
    }

    /// Descriptors for all kinds: registered drivers as given by the driver (forced to
    /// [`DriverStatus::Available`]), the rest as planned. Ordered like [`DbKind::ALL`].
    pub fn descriptors(&self) -> Vec<DriverDescriptor> {
        DbKind::ALL
            .into_iter()
            .map(|kind| match self.drivers.get(&kind) {
                Some(driver) => {
                    let descriptor = driver.descriptor();
                    // A driver may report that it cannot run here; anything else means usable.
                    let status = match descriptor.status {
                        DriverStatus::Unavailable { .. } => descriptor.status,
                        _ => DriverStatus::Available,
                    };
                    DriverDescriptor {
                        status,
                        ..descriptor
                    }
                }
                None => DriverDescriptor::planned(kind),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Mutex;

    use super::*;
    use crate::{ColumnMeta, EndOutcome, StatementEnd, StatementOutcome, Value, ValueKind};

    /// Script language: statements split by `;`. `SELECT` returns ids 1..=5 in batches,
    /// anything else reports 3 affected rows.
    #[derive(Default)]
    struct MockSession {
        running: Mutex<HashSet<Uuid>>,
        cancelled: Mutex<HashSet<Uuid>>,
    }

    #[async_trait]
    impl Session for MockSession {
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        async fn server_info(&self) -> Result<ServerInfo> {
            Ok(ServerInfo {
                product: "mock".into(),
                version: "1".into(),
                ..Default::default()
            })
        }
        async fn execute(
            &self,
            req: QueryRequest,
            sink: &mut dyn ResultSink,
        ) -> Result<ExecSummary> {
            self.running.lock().unwrap().insert(req.query_id);
            let result = self.run(&req, sink).await;
            self.running.lock().unwrap().remove(&req.query_id);
            self.cancelled.lock().unwrap().remove(&req.query_id);
            result
        }
        async fn cancel(&self, query_id: Uuid) -> Result<()> {
            if self.running.lock().unwrap().contains(&query_id) {
                self.cancelled.lock().unwrap().insert(query_id);
                Ok(())
            } else {
                Err(Error::Other("no such running query".into()))
            }
        }
        async fn list_children(&self, _parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
            Ok(vec![])
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }
    }

    impl MockSession {
        async fn run(&self, req: &QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
            let batch = req.batch_size.unwrap_or(2).max(1) as usize;
            let mut statements = 0;
            for stmt in req.text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                statements += 1;
                if stmt.starts_with("SELECT") {
                    sink.on_columns(vec![ColumnMeta {
                        name: "id".into(),
                        type_name: "int8".into(),
                        value_kind: ValueKind::Int,
                        nullable: Some(false),
                    }])?;
                    let ids: Vec<i64> = (1..=5).collect();
                    for chunk in ids.chunks(batch) {
                        sink.on_rows(chunk.iter().map(|i| vec![Value::Int(*i)]).collect())?;
                        tokio::task::yield_now().await;
                        if self.cancelled.lock().unwrap().contains(&req.query_id) {
                            return Err(Error::Cancelled);
                        }
                    }
                    sink.on_message("notice: done".into())?;
                    sink.on_statement_end(StatementEnd {
                        outcome: EndOutcome::Rows {
                            row_count: 5,
                            truncated: false,
                        },
                        elapsed_ms: 1,
                    })?;
                } else {
                    sink.on_statement_end(StatementEnd {
                        outcome: EndOutcome::Affected { rows: 3 },
                        elapsed_ms: 2,
                    })?;
                }
            }
            Ok(ExecSummary {
                statements,
                elapsed_ms: 3,
            })
        }
    }

    /// Sink that stops with `Cancelled` after the first row batch.
    struct StopAfterFirstBatch(u32);

    impl ResultSink for StopAfterFirstBatch {
        fn on_columns(&mut self, _: Vec<ColumnMeta>) -> Result<()> {
            Ok(())
        }
        fn on_rows(&mut self, _: Vec<Vec<Value>>) -> Result<()> {
            self.0 += 1;
            Err(Error::Cancelled)
        }
        fn on_message(&mut self, _: String) -> Result<()> {
            Ok(())
        }
        fn on_statement_end(&mut self, _: StatementEnd) -> Result<()> {
            Ok(())
        }
    }

    struct MockDriver(DbKind);

    #[async_trait]
    impl Driver for MockDriver {
        fn descriptor(&self) -> DriverDescriptor {
            DriverDescriptor::planned(self.0)
        }
        async fn connect(
            &self,
            _config: &ConnectionConfig,
            _secret: Option<&Secret>,
        ) -> Result<Box<dyn Session>> {
            Ok(Box::new(MockSession::default()))
        }
    }

    #[test]
    fn planned_covers_all_kinds_with_phases() {
        let all = planned_descriptors();
        assert_eq!(all.len(), 12);
        let phase = |k: DbKind| match all.iter().find(|d| d.kind == k).unwrap().status {
            DriverStatus::Planned { phase } => phase,
            _ => panic!("must be planned"),
        };
        assert_eq!(phase(DbKind::Postgres), 1);
        assert_eq!(phase(DbKind::MySql), 1);
        assert_eq!(phase(DbKind::Oracle), 3);
        assert_eq!(phase(DbKind::SqlServer), 3);
        assert_eq!(phase(DbKind::Redis), 4);
        assert_eq!(phase(DbKind::MongoDb), 4);
        assert_eq!(phase(DbKind::ClickHouse), 10);
        assert_eq!(phase(DbKind::ScyllaDb), 10);
        assert_eq!(phase(DbKind::Cassandra), 10);
        assert_eq!(phase(DbKind::CockroachDb), 10);
        assert_eq!(phase(DbKind::YugabyteDb), 10);
        assert_eq!(phase(DbKind::TiDb), 10);
    }

    #[test]
    fn descriptor_serialization() {
        let json = serde_json::to_value(DriverDescriptor::planned(DbKind::Redis)).unwrap();
        assert_eq!(
            json["status"],
            serde_json::json!({"state": "planned", "phase": 4})
        );
        assert_eq!(json["family"], "keyValue");
        assert_eq!(json["defaultPort"], 6379);
        assert_eq!(json["capabilities"]["sql"], false);
        let avail = serde_json::to_value(DriverStatus::Available).unwrap();
        assert_eq!(avail, serde_json::json!({"state": "available"}));
    }

    #[tokio::test]
    async fn registry_marks_registered_available_in_stable_order() {
        let mut reg = DriverRegistry::new();
        assert!(
            reg.descriptors()
                .iter()
                .all(|d| d.status != DriverStatus::Available)
        );
        reg.register(Arc::new(MockDriver(DbKind::MySql)));
        let descriptors = reg.descriptors();
        let kinds: Vec<_> = descriptors.iter().map(|d| d.kind).collect();
        assert_eq!(kinds, DbKind::ALL.to_vec());
        for d in &descriptors {
            let expected = d.kind == DbKind::MySql;
            assert_eq!(
                d.status == DriverStatus::Available,
                expected,
                "{:?}",
                d.kind
            );
        }
        let driver = reg.get(DbKind::MySql).expect("registered");
        let cfg = ConnectionConfig::new(DbKind::MySql, "t", "localhost");
        let session = driver.connect(&cfg, None).await.unwrap();
        session.ping().await.unwrap();
        assert!(reg.get(DbKind::Redis).is_none());
    }

    #[tokio::test]
    async fn streams_script_in_batches_and_collects() {
        let session = MockSession::default();
        let mut req = QueryRequest::new("SELECT id; UPDATE t SET x = 1");
        req.batch_size = Some(2);

        // Observe batching with a recording sink.
        #[derive(Default)]
        struct Batches(Vec<usize>);
        impl ResultSink for Batches {
            fn on_columns(&mut self, _: Vec<ColumnMeta>) -> Result<()> {
                Ok(())
            }
            fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
                self.0.push(rows.len());
                Ok(())
            }
            fn on_message(&mut self, _: String) -> Result<()> {
                Ok(())
            }
            fn on_statement_end(&mut self, _: StatementEnd) -> Result<()> {
                Ok(())
            }
        }
        let mut batches = Batches::default();
        let summary = session.execute(req.clone(), &mut batches).await.unwrap();
        assert_eq!(batches.0, vec![2, 2, 1]);
        assert_eq!(
            summary,
            ExecSummary {
                statements: 2,
                elapsed_ms: 3
            }
        );

        let results = execute_collect(&session, req).await.unwrap();
        assert_eq!(results.len(), 2);
        match &results[0].outcome {
            StatementOutcome::Rows(rs) => {
                assert_eq!(rs.columns.len(), 1);
                assert_eq!(rs.columns[0].name, "id");
                assert!(!rs.truncated);
                let ids: Vec<_> = rs.rows.iter().map(|r| r[0].clone()).collect();
                assert_eq!(ids, (1..=5).map(Value::Int).collect::<Vec<_>>());
            }
            other => panic!("expected rows, got {other:?}"),
        }
        assert_eq!(results[0].messages, vec!["notice: done".to_string()]);
        assert_eq!(results[1].outcome, StatementOutcome::Affected { rows: 3 });
        assert!(results[1].messages.is_empty());
    }

    #[tokio::test]
    async fn sink_error_stops_execution() {
        let session = MockSession::default();
        let mut sink = StopAfterFirstBatch(0);
        let err = session
            .execute(
                QueryRequest::new("SELECT id; UPDATE t SET x = 1"),
                &mut sink,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Cancelled));
        assert_eq!(sink.0, 1, "driver must stop after the first failing batch");
    }

    #[tokio::test]
    async fn cancel_stops_running_query() {
        let session = MockSession::default();
        let req = QueryRequest::new("SELECT id");
        let id = req.query_id;
        assert!(session.cancel(id).await.is_err(), "not running yet");
        let mut sink = CollectingSink::new();
        let (exec, cancel) = tokio::join!(session.execute(req, &mut sink), async {
            tokio::task::yield_now().await;
            session.cancel(id).await
        });
        cancel.expect("cancel of a running query succeeds");
        assert!(matches!(exec, Err(Error::Cancelled)));
    }

    #[test]
    fn statement_end_serializes_camel_case() {
        let end = StatementEnd {
            outcome: EndOutcome::Rows {
                row_count: 5,
                truncated: true,
            },
            elapsed_ms: 9,
        };
        assert_eq!(
            serde_json::to_value(&end).unwrap(),
            serde_json::json!({"outcome": {"kind": "rows", "rowCount": 5, "truncated": true}, "elapsedMs": 9})
        );
    }

    #[test]
    fn unavailable_status_shape_and_copy() {
        let st = DriverStatus::Unavailable {
            reason: UnavailableReason::NativeClientTooOld,
        };
        let copy = st; // Copy
        assert_eq!(
            serde_json::to_value(copy).unwrap(),
            serde_json::json!({"state": "unavailable", "reason": "nativeClientTooOld"})
        );
        assert_eq!(
            serde_json::to_value(UnavailableReason::NativeClientMissing).unwrap(),
            "nativeClientMissing"
        );
        assert_eq!(
            serde_json::from_value::<DriverStatus>(
                serde_json::json!({"state": "unavailable", "reason": "nativeClientMissing"})
            )
            .unwrap(),
            DriverStatus::Unavailable {
                reason: UnavailableReason::NativeClientMissing
            }
        );
    }

    #[tokio::test]
    async fn registry_keeps_unavailable_status() {
        struct Oracle;
        #[async_trait]
        impl Driver for Oracle {
            fn descriptor(&self) -> DriverDescriptor {
                DriverDescriptor::new(
                    DbKind::Oracle,
                    Capabilities::planned_for(DbKind::Oracle),
                    DriverStatus::Unavailable {
                        reason: UnavailableReason::NativeClientMissing,
                    },
                )
            }
            async fn connect(
                &self,
                _: &ConnectionConfig,
                _: Option<&Secret>,
            ) -> Result<Box<dyn Session>> {
                Err(Error::DriverUnavailable("no client".into()))
            }
        }
        let mut reg = DriverRegistry::new();
        reg.register(Arc::new(Oracle));
        let d = reg
            .descriptors()
            .into_iter()
            .find(|d| d.kind == DbKind::Oracle)
            .unwrap();
        assert!(matches!(d.status, DriverStatus::Unavailable { .. }));
    }

    #[tokio::test]
    async fn new_session_methods_have_defaults() {
        let s = MockSession::default();
        assert_eq!(s.script_dialect(), ScriptDialect::Opaque);
        assert!(matches!(
            s.open_lease(LeaseOptions::default()).await,
            Err(Error::Unsupported(c)) if c == "leases"
        ));
        let changes = ChangeSet::Keys(crate::KeyChangeSet {
            database: ObjectPath::root(),
            changes: vec![],
        });
        assert!(matches!(
            s.preview_changes(&changes).await,
            Err(Error::Unsupported(c)) if c == "data_editing"
        ));
        assert!(matches!(
            s.apply_changes(changes).await,
            Err(Error::Unsupported(c)) if c == "data_editing"
        ));
        assert!(matches!(
            s.read_key(KeyReadRequest {
                key: ObjectPath::root(),
                cursor: None,
                count: 1,
                pattern: None,
                max_string_bytes: None
            })
            .await,
            Err(Error::Unsupported(c)) if c == "key_value"
        ));
        assert!(s.capabilities().is_none());
        assert!(matches!(
            s.scan_keys(KeyScanRequest {
                database: ObjectPath::root(),
                pattern: None,
                cursor: None,
                count: 10,
                type_filter: None
            })
            .await,
            Err(Error::Unsupported(c)) if c == "scan_keys"
        ));
        assert!(matches!(
            s.generate_ddl(DdlRequest {
                scope: ObjectPath::root(),
                objects: vec![],
                options: Default::default()
            })
            .await,
            Err(Error::Unsupported(c)) if c == "ddl_generation"
        ));
        assert!(matches!(
            s.explain(ExplainRequest {
                query_id: Uuid::nil(),
                text: "SELECT 1".into(),
                database: None,
                analyze: false,
                options: Default::default(),
                timeout_ms: None
            })
            .await,
            Err(Error::Unsupported(c)) if c == "explain"
        ));
    }
}
