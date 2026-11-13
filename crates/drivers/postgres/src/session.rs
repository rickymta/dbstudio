//! The [`Session`] implementation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dbstudio_core::{
    Error, ExecSummary, ObjectKind, ObjectNode, ObjectPath, QueryRequest, Result, ResultSink,
    ServerInfo, Session, TableCountRequest, TableDataRequest, TableDetails,
};
use tokio_postgres::CancelToken;
use uuid::Uuid;

use crate::conn::{PooledConn, Shared};
use crate::error::map_error;
use crate::exec::{self, DEFAULT_BATCH, Running};
use crate::sql;
use crate::{browse, ddl, details};

struct Registration {
    running: Arc<Running>,
    token: CancelToken,
}

/// A live PostgreSQL server session (several pooled connections behind one object).
pub struct PostgresSession {
    shared: Arc<Shared>,
    running: Mutex<HashMap<Uuid, Registration>>,
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
        };
        // Connect eagerly so that bad credentials, TLS problems and unreachable hosts are
        // reported by `Driver::connect`.
        let mut conn = session.shared.acquire(&session.shared.login_db).await?;
        conn.finish().await;
        Ok(session)
    }

    fn register(&self, id: Uuid, conn: &PooledConn) -> (Arc<Running>, Unregister<'_>) {
        let running = Arc::new(Running::default());
        if let Ok(mut map) = self.running.lock() {
            map.insert(
                id,
                Registration {
                    running: running.clone(),
                    token: conn.cancel.clone(),
                },
            );
        }
        (running, Unregister { session: self, id })
    }

    /// Cancel on the server and fail with `Timeout`.
    async fn timeout_cleanup(&self, running: &Running, token: &CancelToken) {
        running
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.shared.cancel_token(token).await;
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
    async fn ping(&self) -> Result<()> {
        let mut conn = self.shared.acquire(self.login_db()).await?;
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
        let mut conn = self.shared.acquire(self.login_db()).await?;
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
        let (running, _unregister) = self.register(req.query_id, &conn);
        let token = conn.cancel.clone();
        let started = Instant::now();
        let batch = batch_size(req.batch_size);
        let run = exec::run_script(&mut conn, &req.text, batch, req.max_rows, sink, &running);
        let result = match req.timeout_ms {
            Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), run).await {
                Ok(r) => r,
                Err(_) => {
                    // The connection is mid-statement: cancel it on the server and drop it.
                    self.timeout_cleanup(&running, &token).await;
                    return Err(Error::Timeout);
                }
            },
            None => run.await,
        };
        match result {
            Ok(statements) => {
                conn.finish().await;
                Ok(ExecSummary {
                    statements,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                })
            }
            Err(e) => {
                if matches!(e, Error::Query { .. } | Error::Cancelled) {
                    conn.finish().await;
                }
                Err(e)
            }
        }
    }

    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        let (running, token) = {
            let map = self.running.lock().expect("running map poisoned");
            match map.get(&query_id) {
                Some(r) => (r.running.clone(), r.token.clone()),
                None => return Err(Error::NotFound(format!("no running query {query_id}"))),
            }
        };
        running
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        self.shared.cancel_token(&token).await
    }

    async fn list_children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
        browse::list_children(&self.shared, parent).await
    }

    async fn object_ddl(&self, path: &ObjectPath) -> Result<String> {
        let mut conn = self.shared.acquire(self.database_of(path)).await?;
        let r = ddl::object_ddl(&conn.client, path).await;
        conn.finish().await;
        r
    }

    async fn table_details(&self, path: &ObjectPath) -> Result<TableDetails> {
        let mut conn = self.shared.acquire(self.database_of(path)).await?;
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
        let select = sql::select_sql(
            &table,
            &req.filters,
            req.raw_where.as_deref(),
            &req.sort,
            req.offset,
            req.limit,
        )?;
        let mut conn = self.shared.acquire(&target.database).await?;
        let (running, _unregister) = self.register(req.query_id, &conn);
        let started = Instant::now();
        let result = exec::run_table_select(
            &mut conn,
            &select,
            batch_size(req.batch_size),
            sink,
            &running,
        )
        .await;
        match result {
            Ok(()) => {
                conn.finish().await;
                Ok(ExecSummary {
                    statements: 1,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                })
            }
            Err(e) => {
                if matches!(e, Error::Query { .. } | Error::Cancelled) {
                    conn.finish().await;
                }
                Err(e)
            }
        }
    }

    async fn count_table(&self, req: TableCountRequest) -> Result<u64> {
        let target = details::resolve_relation(&req.table)?;
        let table = sql::qualified(&target.schema, &target.name);
        let select = sql::count_sql(&table, &req.filters, req.raw_where.as_deref())?;
        let mut conn = self.shared.acquire(&target.database).await?;
        let r = exec::run_count(&mut conn, &select).await;
        if matches!(r, Ok(_) | Err(Error::Query { .. })) {
            conn.finish().await;
        }
        r
    }

    async fn close(&self) -> Result<()> {
        let tokens: Vec<CancelToken> = self
            .running
            .lock()
            .map(|m| m.values().map(|r| r.token.clone()).collect())
            .unwrap_or_default();
        self.shared.close();
        for token in tokens {
            let _ = self.shared.cancel_token(&token).await;
        }
        Ok(())
    }
}
