//! The [`Session`] implementation. ClickHouse over HTTP is stateless: each statement is one
//! request, so a session is just the shared [`Core`] plus the registry of running calls.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dbstudio_core::{
    ApplyResult, ChangeSet, DdlItem, DdlRequest, Editability, Error, ExecSummary, ExplainRequest,
    Lease, LeaseOptions, ObjectNode, ObjectPath, ObjectSearchRequest, QueryRequest, RawPlan,
    Result, ResultSink, SchemaModel, SchemaModelRequest, ScriptDialect, ServerInfo, Session,
    TableCountRequest, TableDataRequest, TableDetails,
};
use uuid::Uuid;

use crate::conn::{Core, Req, cell_u64};
use crate::exec::{Call, Registry, run_script};
use crate::sql::{count_sql, select_sql, table_target};
use crate::{browse, edit, explain, lease};

/// ClickHouse session.
pub struct ClickHouseSession {
    pub(crate) core: Arc<Core>,
    reg: Arc<Registry>,
    info: ServerInfo,
    closed: AtomicBool,
}

impl std::fmt::Debug for ClickHouseSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickHouseSession")
            .field("version", &self.info.version)
            .finish_non_exhaustive()
    }
}

impl ClickHouseSession {
    pub(crate) fn new(core: Arc<Core>, info: ServerInfo) -> Self {
        ClickHouseSession {
            core,
            reg: Arc::new(Registry::default()),
            info,
            closed: AtomicBool::new(false),
        }
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            Err(Error::Connection("the session is closed".into()))
        } else {
            Ok(())
        }
    }

    async fn run(
        &self,
        req: QueryRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let guard = self.reg.register(req.query_id);
        let call = Call {
            core: &self.core,
            ctl: &guard.ctl,
            session_id: None,
            timeout: req.timeout_ms.map(Duration::from_millis),
            started: Instant::now(),
        };
        let mut db = self.core.default_db.clone();
        run_script(&call, &mut db, &req, None, sink).await
    }
}

#[async_trait]
impl Session for ClickHouseSession {
    async fn ping(&self) -> Result<()> {
        self.ensure_open()?;
        self.core.exec(&Req::default(), "SELECT 1 FORMAT Null").await
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        Ok(self.info.clone())
    }

    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        self.ensure_open()?;
        self.run(req, sink).await
    }

    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        self.reg.cancel(&self.core, query_id).await
    }

    async fn list_children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
        self.ensure_open()?;
        browse::list_children(&self.core, parent).await
    }

    async fn object_ddl(&self, path: &ObjectPath) -> Result<String> {
        self.ensure_open()?;
        browse::object_ddl(&self.core, path).await
    }

    async fn table_details(&self, path: &ObjectPath) -> Result<TableDetails> {
        self.ensure_open()?;
        browse::table_details(&self.core, path).await
    }

    async fn fetch_table(
        &self,
        req: TableDataRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        self.ensure_open()?;
        let (db, table) = table_target(&req.table)?;
        let facts = browse::table_facts(&self.core, &db, &table).await?;
        let sql = select_sql(&db, &table, &req, &facts.sorting_key)?;
        let q = QueryRequest {
            text: sql,
            query_id: req.query_id,
            max_rows: None,
            timeout_ms: None,
            batch_size: req.batch_size,
            database: None,
        };
        self.run(q, sink).await
    }

    async fn count_table(&self, req: TableCountRequest) -> Result<u64> {
        self.ensure_open()?;
        let (db, table) = table_target(&req.table)?;
        let sql = count_sql(&db, &table, &req)?;
        let id = req.query_id.unwrap_or_else(Uuid::new_v4);
        let guard = self.reg.register(id);
        let request = Req::default().with_id(id);
        let rows = tokio::select! {
            r = self.core.rows(&request, &sql) => r?,
            () = guard.ctl.cancelled_pub() => {
                let _ = self.core.kill(&id.to_string()).await;
                return Err(Error::Cancelled);
            }
        };
        rows.first()
            .and_then(|r| cell_u64(r, 0))
            .ok_or_else(|| Error::Other("unexpected count() result".into()))
    }

    async fn search_objects(&self, req: ObjectSearchRequest) -> Result<Vec<ObjectNode>> {
        self.ensure_open()?;
        browse::search_objects(&self.core, &req).await
    }

    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect::ClickHouse
    }

    async fn open_lease(&self, opts: LeaseOptions) -> Result<Box<dyn Lease>> {
        self.ensure_open()?;
        lease::open(self.core.clone(), opts).await
    }

    async fn editability(&self, path: &ObjectPath) -> Result<Editability> {
        self.ensure_open()?;
        edit::editability(&self.core, path).await
    }

    async fn preview_changes(&self, changes: &ChangeSet) -> Result<Vec<String>> {
        self.ensure_open()?;
        edit::preview(&self.core, changes).await
    }

    async fn apply_changes(&self, changes: ChangeSet) -> Result<ApplyResult> {
        self.ensure_open()?;
        edit::apply(&self.core, changes).await
    }

    async fn explain(&self, req: ExplainRequest) -> Result<RawPlan> {
        self.ensure_open()?;
        explain::explain(&self.core, &self.reg, None, &req).await
    }

    async fn schema_model(&self, req: SchemaModelRequest) -> Result<SchemaModel> {
        self.ensure_open()?;
        browse::schema_model(&self.core, req).await
    }

    async fn generate_ddl(&self, req: DdlRequest) -> Result<Vec<DdlItem>> {
        self.ensure_open()?;
        browse::generate_ddl(&self.core, req).await
    }

    async fn close(&self) -> Result<()> {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.reg.cancel_all(&self.core).await;
        }
        Ok(())
    }
}
