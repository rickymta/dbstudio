//! PostgreSQL driver: dedicated connection (lease) for editor tabs, built on
//! [`dbstudio_core::lease_kit`].
//!
//! * The connection lives outside the pool and is owned by one actor task.
//! * Cancelling sends a protocol cancel request (`CancelToken`) on a short-lived connection; the
//!   statement then fails with `57014` which is reported as `Error::Cancelled`. A cancel inside a
//!   transaction block leaves it aborted (`TxStatus::Failed`), as PostgreSQL does.
//! * PostgreSQL cannot report the transaction state cheaply, so it is tracked from the
//!   statements that ran (`Conn::in_txn` / `Conn::failed`).
//! * Manual commit mode opens the transaction lazily before the first statement.
//! * A result cut short by `max_rows` stays open as a cursor (`fetch_more`); in auto-commit mode
//!   it holds a transaction of its own, which is not reported as the user's transaction.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dbstudio_core::lease_kit::{ActorLease, Canceller, LeaseBackend};
use dbstudio_core::{
    Error, ExecSummary, IsolationLevel, Lease, LeaseContext, LeaseOptions, QueryRequest, Result,
    ResultSink, TxControl, TxOptions, TxStatus,
};
use tokio_postgres::CancelToken;

use crate::conn::{Conn, Shared};
use crate::error::map_error;
use crate::exec::{self, DEFAULT_BATCH, PendingCursor, Running, ScriptOpts};
use crate::session::PostgresSession;
use crate::sql::quote_ident;

/// Longest the final `ROLLBACK` of `close` may take.
const ROLLBACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Open a dedicated connection.
pub(crate) async fn open(session: &PostgresSession, opts: LeaseOptions) -> Result<Box<dyn Lease>> {
    let shared = session.shared.clone();
    let database = opts
        .database
        .clone()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| shared.login_db.clone());
    let conn = shared.open_dedicated(&database).await?;
    let mut backend = PgBackend {
        token: conn.cancel.clone(),
        shared,
        conn: Some(conn),
        database: database.clone(),
        auto_commit: opts.auto_commit,
        pending: None,
        current: Arc::new(Mutex::new(None)),
    };
    if let Some(schema) = opts.schema.as_deref().filter(|s| !s.is_empty()) {
        backend.set_search_path(schema).await?;
    }
    let schema = backend.query_schema().await.or_else(|| opts.schema.clone());
    let initial = LeaseOptions {
        database: Some(database),
        schema,
        auto_commit: opts.auto_commit,
    };
    Ok(Box::new(ActorLease::spawn(backend, initial)?))
}

struct PgBackend {
    shared: Arc<Shared>,
    conn: Option<Conn>,
    token: CancelToken,
    database: String,
    auto_commit: bool,
    pending: Option<PendingCursor>,
    /// Cancellation state of the call that is running right now.
    current: Arc<Mutex<Option<Arc<Running>>>>,
}

struct PgCanceller {
    shared: Arc<Shared>,
    token: CancelToken,
    current: Arc<Mutex<Option<Arc<Running>>>>,
}

#[async_trait]
impl Canceller for PgCanceller {
    async fn cancel(&self) -> Result<()> {
        let running = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match running {
            Some(running) => running.send_cancel(&self.shared, &self.token).await,
            None => Ok(()),
        }
    }
}

fn lost() -> Error {
    Error::Connection("the connection is closed".into())
}

fn isolation_sql(level: IsolationLevel) -> Result<&'static str> {
    Ok(match level {
        IsolationLevel::ReadUncommitted => "READ UNCOMMITTED",
        IsolationLevel::ReadCommitted => "READ COMMITTED",
        IsolationLevel::RepeatableRead => "REPEATABLE READ",
        IsolationLevel::Serializable => "SERIALIZABLE",
        IsolationLevel::Snapshot => {
            return Err(Error::Unsupported(
                "snapshot isolation on PostgreSQL (use repeatable read)".into(),
            ));
        }
    })
}

impl PgBackend {
    fn conn(&mut self) -> Result<&mut Conn> {
        match self.conn.as_mut() {
            Some(c) if !c.client.is_closed() => Ok(c),
            _ => Err(lost()),
        }
    }

    /// After a failure: a dead socket is a connection error whatever the driver said.
    fn classify(&self, e: Error) -> Error {
        match (&self.conn, &e) {
            (Some(c), _) if c.client.is_closed() && !matches!(e, Error::Connection(_)) => lost(),
            _ => e,
        }
    }

    async fn set_search_path(&mut self, schema: &str) -> Result<()> {
        let sql = format!("SET search_path TO {}", quote_ident(schema));
        self.conn()?
            .client
            .batch_execute(&sql)
            .await
            .map_err(map_error)
    }

    /// `current_schema()`, `None` when it cannot be read (aborted transaction, no schema).
    async fn query_schema(&mut self) -> Option<String> {
        let conn = self.conn.as_ref()?;
        let row = conn
            .client
            .query_one("SELECT current_schema()::text", &[])
            .await
            .ok()?;
        row.get::<_, Option<String>>(0)
    }

    async fn drop_pending(&mut self) {
        if let Some(pending) = self.pending.take() {
            if let Some(conn) = self.conn.as_mut() {
                let _ = exec::discard_pending(conn, pending).await;
            }
        }
    }

    fn begin_call(&self) -> Arc<Running> {
        let running = Arc::new(Running::default());
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(running.clone());
        running
    }

    async fn end_call(&self, running: &Running) {
        running.retire().await;
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[async_trait]
impl LeaseBackend for PgBackend {
    async fn execute(
        &mut self,
        req: QueryRequest,
        sink: &mut dyn ResultSink,
        keep_open: bool,
    ) -> Result<ExecSummary> {
        self.drop_pending().await;
        self.conn()?;
        let running = self.begin_call();
        let started = Instant::now();
        let opts = ScriptOpts {
            batch: req
                .batch_size
                .map_or(DEFAULT_BATCH, |b| b as usize)
                .clamp(1, 100_000),
            max_rows: req.max_rows,
            lazy_begin: !self.auto_commit,
            keep_open,
        };
        let conn = self.conn.as_mut().ok_or_else(lost)?;
        let result =
            exec::run_script_ex(conn, &req.text, &opts, sink, &running, &mut self.pending).await;
        self.end_call(&running).await;
        match result {
            Ok(statements) => Ok(ExecSummary {
                statements,
                elapsed_ms: started.elapsed().as_millis() as u64,
            }),
            Err(e) => Err(self.classify(e)),
        }
    }

    async fn fetch_more(
        &mut self,
        max_rows: u64,
        batch: usize,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let pending = self
            .pending
            .take()
            .ok_or_else(|| Error::NotFound("pending result".into()))?;
        self.conn()?;
        let running = self.begin_call();
        let started = Instant::now();
        let conn = self.conn.as_mut().ok_or_else(lost)?;
        let result = exec::fetch_pending(
            conn,
            pending,
            Some(max_rows),
            batch.clamp(1, 100_000),
            sink,
            &running,
        )
        .await;
        self.end_call(&running).await;
        match result {
            Ok(rest) => {
                self.pending = rest;
                Ok(ExecSummary {
                    statements: 1,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                })
            }
            Err(e) => {
                if let Some(conn) = self.conn.as_mut() {
                    if conn.in_txn && matches!(e, Error::Query { .. } | Error::Cancelled) {
                        conn.failed = true;
                    }
                }
                Err(self.classify(e))
            }
        }
    }

    async fn discard_result(&mut self) -> Result<()> {
        self.drop_pending().await;
        Ok(())
    }

    async fn tx(&mut self, op: TxControl, opts: TxOptions) -> Result<()> {
        self.drop_pending().await;
        let sql = match op {
            TxControl::Begin => {
                let mut sql = String::from("BEGIN");
                if let Some(level) = opts.isolation {
                    sql.push_str(" ISOLATION LEVEL ");
                    sql.push_str(isolation_sql(level)?);
                }
                if opts.read_only {
                    sql.push_str(" READ ONLY");
                }
                sql
            }
            TxControl::Commit => "COMMIT".to_string(),
            TxControl::Rollback => "ROLLBACK".to_string(),
            other => {
                return Err(Error::Unsupported(format!(
                    "transaction control {other:?} (send the statement instead)"
                )));
            }
        };
        let conn = self.conn()?;
        conn.client
            .batch_execute(&sql)
            .await
            .map_err(map_error)
            .map_err(|e| match &self.conn {
                Some(c) if c.client.is_closed() => lost(),
                _ => e,
            })?;
        let conn = self.conn()?;
        conn.in_txn = op == TxControl::Begin;
        conn.failed = false;
        Ok(())
    }

    async fn set_auto_commit(&mut self, on: bool) -> Result<()> {
        self.drop_pending().await;
        self.auto_commit = on;
        Ok(())
    }

    async fn set_context(&mut self, ctx: LeaseContext) -> Result<()> {
        self.drop_pending().await;
        if let Some(db) = ctx.database.as_deref() {
            if db != self.database {
                return Err(Error::Unsupported(
                    "switching database on PostgreSQL (open the editor on that database)".into(),
                ));
            }
        }
        if let Some(schema) = ctx.schema.as_deref() {
            self.set_search_path(schema).await?;
        }
        Ok(())
    }

    async fn probe_tx(&mut self) -> Result<TxStatus> {
        let Some(conn) = self.conn.as_ref() else {
            return Ok(TxStatus::Idle);
        };
        if self.pending.as_ref().is_some_and(PendingCursor::own_txn) || !conn.in_txn {
            return Ok(TxStatus::Idle);
        }
        Ok(if conn.failed {
            TxStatus::Failed
        } else {
            TxStatus::Active
        })
    }

    async fn refresh_context(&mut self, executed_text: &str) -> Result<Option<LeaseContext>> {
        if !executed_text.to_ascii_lowercase().contains("search_path") {
            return Ok(None);
        }
        // Not possible inside an aborted transaction block.
        if self.conn.as_ref().is_some_and(|c| c.failed) {
            return Ok(None);
        }
        Ok(self.query_schema().await.map(|schema| LeaseContext {
            database: None,
            schema: Some(schema),
        }))
    }

    async fn ping(&mut self) -> Result<()> {
        let failed = self.conn.as_ref().is_some_and(|c| c.failed);
        let conn = self.conn()?;
        if failed {
            // Any statement is refused in an aborted transaction block; the socket check is
            // all that can be done.
            return Ok(());
        }
        conn.client
            .simple_query("SELECT 1")
            .await
            .map(|_| ())
            .map_err(map_error)
            .map_err(|e| match &self.conn {
                Some(c) if c.client.is_closed() => lost(),
                _ => e,
            })
    }

    async fn close(&mut self) -> Result<bool> {
        let rolled_back = self.probe_tx().await? != TxStatus::Idle;
        if let Some(mut conn) = self.conn.take() {
            if !conn.client.is_closed() && conn.in_txn {
                let _ =
                    tokio::time::timeout(ROLLBACK_TIMEOUT, conn.client.batch_execute("ROLLBACK"))
                        .await;
                conn.in_txn = false;
            }
            // Dropping the client closes the socket.
        }
        self.pending = None;
        Ok(rolled_back)
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(PgCanceller {
            shared: self.shared.clone(),
            token: self.token.clone(),
            current: self.current.clone(),
        })
    }
}
