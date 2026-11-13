//! The [`Session`] implementation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dbstudio_core::{
    ApplyResult, ChangeSet, DdlItem, DdlRequest, Editability, ExplainRequest, Lease, LeaseOptions,
    RawPlan, SchemaModel, SchemaModelRequest, ScriptDialect,
};
use dbstudio_core::{
    Error, ExecSummary, ObjectKind, ObjectNode, ObjectPath, ObjectSearchRequest, QueryRequest,
    Result, ResultSink, ServerInfo, Session, TableCountRequest, TableDataRequest, TableDetails,
};
use tokio_postgres::CancelToken;
use uuid::Uuid;

use crate::conn::{PooledConn, Shared};
use crate::error::map_error;
use crate::exec::{self, DEFAULT_BATCH, Running};
use crate::search;
use crate::sql;
use crate::{browse, ddl, details};

struct Registration {
    running: Arc<Running>,
    token: CancelToken,
}

/// A live PostgreSQL server session (several pooled connections behind one object).
pub struct PostgresSession {
    pub(crate) shared: Arc<Shared>,
    running: Mutex<HashMap<Uuid, Registration>>,
    /// Cached unique-key columns per table, used as ORDER BY tie-breakers (value expires).
    keys: Mutex<KeyCache>,
}

type KeyCache = HashMap<(String, String, String), (Instant, Vec<String>)>;

const KEY_CACHE_TTL: Duration = Duration::from_secs(30);

fn strip_position(e: Error) -> Error {
    match e {
        Error::Query { message, .. } => Error::Query {
            message,
            position: None,
        },
        other => other,
    }
}

impl std::fmt::Debug for PostgresSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresSession")
            .field("login_db", &self.shared.login_db)
            .finish_non_exhaustive()
    }
}

/// Removes the cancel registration when the request ends (also when its future is dropped).
struct Unregister<'a> {
    session: &'a PostgresSession,
    id: Uuid,
}

impl Drop for Unregister<'_> {
    fn drop(&mut self) {
        if let Ok(mut map) = self.session.running.lock() {
            map.remove(&self.id);
        }
    }
}

fn batch_size(requested: Option<u32>) -> usize {
    requested
        .map(|b| b as usize)
        .unwrap_or(DEFAULT_BATCH)
        .clamp(1, 100_000)
}

impl PostgresSession {
    pub(crate) async fn new(shared: Shared) -> Result<Self> {
        let session = PostgresSession {
            shared: Arc::new(shared),
            running: Mutex::new(HashMap::new()),
            keys: Mutex::new(HashMap::new()),
        };
        // Connect eagerly so that bad credentials, TLS problems and unreachable hosts are
        // reported by `Driver::connect`.
        let mut conn = session.shared.acquire(&session.shared.login_db).await?;
        conn.finish().await;
        Ok(session)
    }

    fn register(
        &self,
        id: Option<Uuid>,
        conn: &PooledConn,
    ) -> (Arc<Running>, Option<Unregister<'_>>) {
        let running = Arc::new(Running::default());
        let Some(id) = id else {
            return (running, None);
        };
        if let Ok(mut map) = self.running.lock() {
            map.insert(
                id,
                Registration {
                    running: running.clone(),
                    token: conn.cancel.clone(),
                },
            );
        }
        (running, Some(Unregister { session: self, id }))
    }

    /// Retire the request and return the connection to the pool when it is clean. A connection
    /// that has had (or is receiving) a cancel request is never reused.
    async fn end_request(&self, conn: &mut PooledConn, running: &Running, clean: bool) {
        let cancelled = running.retire().await;
        if clean && !cancelled {
            conn.finish().await;
        }
    }

    /// Key columns used to make paging deterministic.
    async fn table_key(&self, conn: &PooledConn, rel: &details::Relation) -> Vec<String> {
        let id = (rel.database.clone(), rel.schema.clone(), rel.name.clone());
        if let Ok(map) = self.keys.lock() {
            if let Some((at, cols)) = map.get(&id) {
                if at.elapsed() < KEY_CACHE_TTL {
                    return cols.clone();
                }
            }
        }
        let cols = details::key_columns(&conn.client, &rel.schema, &rel.name)
            .await
            .unwrap_or_default();
        if let Ok(mut map) = self.keys.lock() {
            map.insert(id, (Instant::now(), cols.clone()));
        }
        cols
    }

    fn forget_key(&self, rel: &details::Relation) {
        if let Ok(mut map) = self.keys.lock() {
            map.remove(&(rel.database.clone(), rel.schema.clone(), rel.name.clone()));
        }
    }

    fn login_db(&self) -> &str {
        &self.shared.login_db
    }

    /// Database named by the first `Database` segment of `path`.
    fn database_of<'a>(&'a self, path: &'a ObjectPath) -> &'a str {
        path.segments
            .iter()
            .find(|s| s.kind == ObjectKind::Database)
            .map(|s| s.name.as_str())
            .unwrap_or_else(|| self.login_db())
    }
}

#[async_trait]
impl Session for PostgresSession {
    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect::Postgres
    }

    async fn open_lease(&self, opts: LeaseOptions) -> Result<Box<dyn Lease>> {
        crate::lease::open(self, opts).await
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

    async fn ping(&self) -> Result<()> {
        let mut conn = self.shared.acquire_catalog(self.login_db()).await?;
        let r = conn
            .client
            .simple_query("SELECT 1")
            .await
            .map_err(map_error);
        if r.is_ok() {
            conn.finish().await;
        }
        r.map(|_| ())
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        let mut conn = self.shared.acquire_catalog(self.login_db()).await?;
        let row = conn
            .client
            .query_one(
                "SELECT version(), current_setting('server_version'), \
                        current_setting('server_version_num'), current_setting('server_encoding'), \
                        current_database()::text, current_user::text",
                &[],
            )
            .await
            .map_err(map_error)?;
        conn.finish().await;
        let full: String = row.get(1);
        let short = full.split_whitespace().next().unwrap_or(&full).to_string();
        let mut info = ServerInfo {
            product: "PostgreSQL".to_string(),
            version: short,
            ..Default::default()
        };
        info.extra
            .insert("fullVersion".into(), row.get::<_, String>(0));
        info.extra.insert("serverVersion".into(), full);
        info.extra
            .insert("versionNum".into(), row.get::<_, String>(2));
        info.extra
            .insert("encoding".into(), row.get::<_, String>(3));
        info.extra
            .insert("database".into(), row.get::<_, String>(4));
        info.extra.insert("user".into(), row.get::<_, String>(5));
        if let Some(tz) = self.shared.server_timezone() {
            info.extra.insert("timezone".into(), tz.to_string());
        }
        Ok(info)
    }

    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        let db = req
            .database
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| self.login_db().to_string());
        let mut conn = self.shared.acquire(&db).await?;
        let (running, _unregister) = self.register(Some(req.query_id), &conn);
        let token = conn.cancel.clone();
        let started = Instant::now();
        let batch = batch_size(req.batch_size);
        let run = exec::run_script(&mut conn, &req.text, batch, req.max_rows, sink, &running);
        let result = match req.timeout_ms {
            Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), run).await {
                Ok(r) => r,
                Err(_) => {
                    // The connection is mid-statement: cancel it on the server and drop it.
                    let _ = running.send_cancel(&self.shared, &token).await;
                    return Err(Error::Timeout);
                }
            },
            None => run.await,
        };
        match result {
            Ok(statements) => {
                self.end_request(&mut conn, &running, true).await;
                Ok(ExecSummary {
                    statements,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                })
            }
            Err(e) => {
                let clean = matches!(e, Error::Query { .. } | Error::Cancelled);
                self.end_request(&mut conn, &running, clean).await;
                Err(e)
            }
        }
    }

    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        let found = {
            let map = self.running.lock().expect("running map poisoned");
            map.get(&query_id)
                .map(|r| (r.running.clone(), r.token.clone()))
        };
        // Unknown or already finished: nothing to cancel.
        match found {
            Some((running, token)) => running.send_cancel(&self.shared, &token).await,
            None => Ok(()),
        }
    }

    async fn list_children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
        browse::list_children(&self.shared, parent).await
    }

    async fn object_ddl(&self, path: &ObjectPath) -> Result<String> {
        let mut conn = self.shared.acquire_catalog(self.database_of(path)).await?;
        let r = ddl::object_ddl(&conn.client, path).await;
        conn.finish().await;
        r
    }

    async fn table_details(&self, path: &ObjectPath) -> Result<TableDetails> {
        let mut conn = self.shared.acquire_catalog(self.database_of(path)).await?;
        let r = details::table_details(&conn.client, path, self.database_of(path)).await;
        conn.finish().await;
        r
    }

    async fn fetch_table(
        &self,
        req: TableDataRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let target = details::resolve_relation(&req.table)?;
        let table = sql::qualified(&target.schema, &target.name);
        let mut conn = self.shared.acquire(&target.database).await?;
        let key = self.table_key(&conn, &target).await;
        let select = match sql::select_sql(
            &table,
            &req.filters,
            req.raw_where.as_deref(),
            &req.sort,
            &key,
            req.offset,
            req.limit,
        ) {
            Ok(s) => s,
            Err(e) => {
                conn.finish().await;
                return Err(e);
            }
        };
        let (running, _unregister) = self.register(Some(req.query_id), &conn);
        let started = Instant::now();
        let result = exec::run_table_select(
            &mut conn,
            &select,
            batch_size(req.batch_size),
            sink,
            &running,
        )
        .await
        .map_err(strip_position);
        if result.is_err() {
            self.forget_key(&target);
        }
        let clean = matches!(result, Ok(()) | Err(Error::Query { .. } | Error::Cancelled));
        self.end_request(&mut conn, &running, clean).await;
        result.map(|()| ExecSummary {
            statements: 1,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    async fn count_table(&self, req: TableCountRequest) -> Result<u64> {
        let target = details::resolve_relation(&req.table)?;
        let table = sql::qualified(&target.schema, &target.name);
        let select = sql::count_sql(&table, &req.filters, req.raw_where.as_deref())?;
        let mut conn = self.shared.acquire(&target.database).await?;
        let (running, _unregister) = self.register(req.query_id, &conn);
        let result = exec::run_count(&mut conn, &select, &running)
            .await
            .map_err(strip_position);
        let clean = matches!(result, Ok(_) | Err(Error::Query { .. } | Error::Cancelled));
        self.end_request(&mut conn, &running, clean).await;
        result
    }

    async fn search_objects(&self, req: ObjectSearchRequest) -> Result<Vec<ObjectNode>> {
        let scope = search::parse_scope(&req)?;
        let database = scope
            .database
            .clone()
            .unwrap_or_else(|| self.login_db().to_string());
        let mut conn = self.shared.acquire_catalog(&database).await?;
        let r = search::search(
            &conn.client,
            &req,
            &scope,
            &database,
            self.shared.search_timeout,
        )
        .await;
        if r.is_ok() || matches!(r, Err(Error::Query { .. } | Error::Timeout)) {
            conn.mark_reusable();
        }
        r
    }

    async fn close(&self) -> Result<()> {
        let active: Vec<(Arc<Running>, CancelToken)> = self
            .running
            .lock()
            .map(|m| {
                m.values()
                    .map(|r| (r.running.clone(), r.token.clone()))
                    .collect()
            })
            .unwrap_or_default();
        // Flag every running request first so that they all end as `Cancelled`.
        for (running, _) in &active {
            running
                .cancelled
                .store(true, std::sync::atomic::Ordering::Release);
        }
        self.shared.close();
        for (running, token) in &active {
            let _ = running.send_cancel(&self.shared, token).await;
        }
        Ok(())
    }
}
