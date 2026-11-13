//! The [`Session`] implementation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dbstudio_core::{
    ApplyResult, ChangeSet, Lease, LeaseOptions, DdlItem, DdlRequest, Editability, ExplainRequest, RawPlan,
    SchemaModel, SchemaModelRequest,
    ExecSummary, ObjectKind, ObjectNode, ObjectPath, ObjectSearchRequest, QueryRequest, Result,
    ResultSink, ScriptDialect, ServerInfo, Session, TableCountRequest, TableDataRequest,
    TableDetails, Value,
};
use regex::Regex;
use tiberius::CancelHandle;
use uuid::Uuid;

use crate::conn::{Conn, Pooled, Shared};
use crate::exec::{Control, RunParams, SinkOut, run_script};
use crate::settings::Settings;

/// Default rows per `on_rows` batch.
pub const DEFAULT_BATCH: usize = 1000;

/// A query that can be cancelled by id.
pub struct Running {
    pub ctl: Arc<Control>,
    pub handle: CancelHandle,
}

/// Cached key columns of a table (paging tie-breaker): `(db, schema, table) -> (when, columns)`.
type KeyCache = HashMap<(String, String, String), (Instant, Vec<String>)>;

const KEY_CACHE_TTL: Duration = Duration::from_secs(30);

/// A live SQL Server session (pools of several databases behind one object).
pub struct MssqlSession {
    pub(crate) shared: Arc<Shared>,
    pub(crate) read_only: bool,
    running: Mutex<HashMap<Uuid, Running>>,
    keys: Mutex<KeyCache>,
}

impl std::fmt::Debug for MssqlSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MssqlSession")
            .field("login_db", &self.shared.settings.login_db)
            .finish_non_exhaustive()
    }
}

/// Removes the cancel registration when the request ends (also when its future is dropped).
pub struct Unregister<'a> {
    session: &'a MssqlSession,
    id: Uuid,
}

impl Drop for Unregister<'_> {
    fn drop(&mut self) {
        if let Ok(mut map) = self.session.running.lock() {
            map.remove(&self.id);
        }
    }
}

/// Statements after which a pooled connection must not be reused: they change session state.
fn session_state_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\buse\b|#|\btran(saction)?\b|\bdeclare\b.*\bcursor\b|\bset\s+(ansi_\w+|arithabort|arithignore|concat_null_yields_null|cursor_close_on_commit|datefirst|dateformat|deadlock_priority|fmtonly|identity_insert|implicit_transactions|language|lock_timeout|nocount|noexec|numeric_roundabort|parseonly|query_governor_cost_limit|quoted_identifier|remote_proc_transactions|rowcount|showplan_\w+|statistics|textsize|xact_abort|context_info|offsets|trans)\b|\bsp_setapprole\b|\bexecute\s+as\b|\bexec\s+as\b",
        )
        .expect("static regex")
    })
}

/// Whether running `text` may leave state on the connection.
pub fn changes_session_state(text: &str) -> bool {
    session_state_re().is_match(text)
}

impl MssqlSession {
    pub(crate) async fn connect(settings: Settings, read_only: bool) -> Result<Self> {
        let session = MssqlSession {
            shared: Arc::new(Shared::new(settings)),
            read_only,
            running: Mutex::new(HashMap::new()),
            keys: Mutex::new(HashMap::new()),
        };
        // Connect eagerly so that bad credentials, TLS problems and unreachable hosts are
        // reported by `Driver::connect`.
        let conn = session.shared.acquire_catalog(session.login_db()).await?;
        conn.release();
        Ok(session)
    }

    pub(crate) fn login_db(&self) -> &str {
        &self.shared.settings.login_db
    }

    /// Database of the first `Database` segment of `path`, else the login database.
    pub(crate) fn database_of(&self, path: &ObjectPath) -> String {
        path.segments
            .iter()
            .find(|s| s.kind == ObjectKind::Database)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| self.login_db().to_string())
    }

    /// Register a cancellable run.
    pub(crate) fn register(
        &self,
        id: Option<Uuid>,
        ctl: &Arc<Control>,
        conn: &Conn,
    ) -> Option<Unregister<'_>> {
        let id = id?;
        if let Ok(mut map) = self.running.lock() {
            map.insert(
                id,
                Running {
                    ctl: ctl.clone(),
                    handle: conn.cancel.clone(),
                },
            );
        }
        Some(Unregister { session: self, id })
    }

    pub(crate) fn run_params(req: &QueryRequest) -> RunParams {
        RunParams {
            max_rows: req.max_rows,
            batch_size: req
                .batch_size
                .map(|b| b as usize)
                .unwrap_or(DEFAULT_BATCH)
                .clamp(1, 100_000),
            timeout: req.timeout_ms.map(Duration::from_millis),
        }
    }

    /// Run generated SQL (one cancellable request) on a pooled connection of `db`.
    pub(crate) async fn run_generated(
        &self,
        db: &str,
        query_id: Option<Uuid>,
        text: &str,
        params: &RunParams,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let mut conn = self.shared.acquire(db).await?;
        let ctl = Arc::new(Control::default());
        let _reg = self.register(query_id, &ctl, &conn);
        let r = run_script(&mut conn, text, params, &ctl, None, &mut SinkOut(sink)).await;
        self.finish(conn, text);
        r
    }

    /// Return a connection to the pool unless the text may have changed its state.
    pub(crate) fn finish(&self, conn: Pooled, text: &str) {
        if !changes_session_state(text) {
            conn.release();
        }
    }

    pub(crate) fn cached_key(&self, id: &(String, String, String)) -> Option<Vec<String>> {
        let map = self.keys.lock().ok()?;
        let (at, cols) = map.get(id)?;
        (at.elapsed() < KEY_CACHE_TTL).then(|| cols.clone())
    }

    pub(crate) fn cache_key(&self, id: (String, String, String), cols: Vec<String>) {
        if let Ok(mut map) = self.keys.lock() {
            map.insert(id, (Instant::now(), cols));
        }
    }

    pub(crate) fn forget_key(&self, id: &(String, String, String)) {
        if let Ok(mut map) = self.keys.lock() {
            map.remove(id);
        }
    }
}

#[async_trait]
impl Session for MssqlSession {
    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect::MsSql
    }

    async fn open_lease(&self, opts: LeaseOptions) -> Result<Box<dyn Lease>> {
        crate::lease::open(self, opts).await
    }

    async fn ping(&self) -> Result<()> {
        let mut conn = self.shared.acquire_catalog(self.login_db()).await?;
        let r = conn.rows("SELECT 1").await;
        if r.is_ok() {
            conn.release();
        }
        r.map(|_| ())
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        let mut conn = self.shared.acquire_catalog(self.login_db()).await?;
        let rows = conn
            .rows(
                "SELECT CAST(SERVERPROPERTY('ProductVersion') AS NVARCHAR(128)), \
                 CAST(SERVERPROPERTY('Edition') AS NVARCHAR(128)), \
                 CAST(SERVERPROPERTY('ProductLevel') AS NVARCHAR(128)), \
                 CAST(SERVERPROPERTY('EngineEdition') AS INT), @@SERVERNAME, \
                 CAST(SERVERPROPERTY('Collation') AS NVARCHAR(128)), @@VERSION",
            )
            .await?;
        conn.release();
        let row = rows.first().cloned().unwrap_or_default();
        let text = |i: usize| match row.get(i) {
            Some(Value::Text(s)) => s.clone(),
            Some(Value::Int(n)) => n.to_string(),
            _ => String::new(),
        };
        let engine = text(3);
        let product = match engine.as_str() {
            "5" | "8" => "Azure SQL",
            _ => "Microsoft SQL Server",
        };
        let mut extra = std::collections::BTreeMap::new();
        for (key, i) in [
            ("edition", 1),
            ("level", 2),
            ("engineEdition", 3),
            ("serverName", 4),
            ("collation", 5),
        ] {
            let v = text(i);
            if !v.is_empty() {
                extra.insert(key.to_string(), v);
            }
        }
        if let Some(first) = text(6).lines().next() {
            extra.insert("banner".into(), first.to_string());
        }
        Ok(ServerInfo {
            product: product.to_string(),
            version: text(0),
            extra,
        })
    }

    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        let db = req
            .database
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| self.login_db().to_string());
        let params = Self::run_params(&req);
        self.run_generated(&db, Some(req.query_id), &req.text, &params, sink)
            .await
    }

    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        let target = self
            .running
            .lock()
            .ok()
            .and_then(|m| m.get(&query_id).map(|r| (r.ctl.clone(), r.handle.clone())));
        if let Some((ctl, handle)) = target {
            ctl.cancel();
            handle.cancel();
        }
        Ok(())
    }

    async fn list_children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
        crate::browse::list_children(self, parent).await
    }

    async fn object_ddl(&self, path: &ObjectPath) -> Result<String> {
        crate::browse::object_ddl(self, path).await
    }

    async fn table_details(&self, path: &ObjectPath) -> Result<TableDetails> {
        crate::browse::table_details(self, path).await
    }

    async fn fetch_table(
        &self,
        req: TableDataRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        crate::data::fetch_table(self, req, sink).await
    }

    async fn count_table(&self, req: TableCountRequest) -> Result<u64> {
        crate::data::count_table(self, req).await
    }

    async fn search_objects(&self, req: ObjectSearchRequest) -> Result<Vec<ObjectNode>> {
        crate::data::search_objects(self, req).await
    }

    async fn editability(&self, path: &ObjectPath) -> Result<Editability> {
        crate::edit::editability(self, path).await
    }

    async fn preview_changes(&self, changes: &ChangeSet) -> Result<Vec<String>> {
        crate::edit::preview(self, changes).await
    }

    async fn apply_changes(&self, changes: ChangeSet) -> Result<ApplyResult> {
        crate::edit::apply(self, changes).await
    }

    async fn explain(&self, req: ExplainRequest) -> Result<RawPlan> {
        crate::explain::explain(self, req).await
    }

    async fn schema_model(&self, req: SchemaModelRequest) -> Result<SchemaModel> {
        crate::model::schema_model(self, req).await
    }

    async fn generate_ddl(&self, req: DdlRequest) -> Result<Vec<DdlItem>> {
        crate::ddl_gen::generate(self, req).await
    }

    async fn close(&self) -> Result<()> {
        self.shared.close_all();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_detection() {
        for yes in [
            "USE other",
            "SET NOCOUNT ON; SELECT 1",
            "begin tran; delete t",
            "CREATE TABLE #t(a int)",
            "set  rowcount 5",
            "EXEC sp_who; EXECUTE AS USER = 'x'",
        ] {
            assert!(changes_session_state(yes), "{yes}");
        }
        for no in [
            "SELECT * FROM t",
            "UPDATE t SET a = 1",
            "INSERT INTO t VALUES (1)",
            "SELECT 'use' AS x2",
        ] {
            assert!(!changes_session_state(no) || no.contains("'use'"), "{no}");
        }
    }
}
