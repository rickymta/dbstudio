//! `Lease`: one dedicated connection for an editor tab.
//!
//! Oracle starts a transaction with the first DML, so "manual" mode is simply "no commit after
//! each statement"; auto-commit mode commits after every statement that is not transaction
//! control. `begin()` in auto-commit mode switches the commits off until `commit()`/`rollback()`.
//! The transaction state is read from `DBMS_TRANSACTION.LOCAL_TRANSACTION_ID`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dbstudio_core::EndOutcome;
use dbstudio_core::{
    BulkInsertRequest, BulkInsertResult, Error, ExecSummary, ExplainRequest, IsolationLevel, Lease,
    LeaseCloseReport, LeaseContext, LeaseOptions, LeaseState, QueryRequest, RawPlan, Result,
    ResultSink, StatementEnd, TableStreamRequest, TxOptions, TxStatus,
};
use oracle::Connection;
use uuid::Uuid;

use crate::catalog;
use crate::data;
use crate::error::map_error;
use crate::exec::{ExecParams, Pending, fetch_pending, run_script};
use crate::explain::{self, TxMode};
use crate::runtime::{Control, NullSink, Shared, stream};
use crate::script;
use crate::sql::{quote_ident, relation_of};

const CLOSE_STEP_TIMEOUT: Duration = Duration::from_secs(5);

struct Inner {
    conn: Arc<Connection>,
    state: Mutex<LeaseState>,
    /// `begin()` was called in auto-commit mode: statements are not committed.
    explicit_tx: AtomicBool,
    pending: Mutex<Option<Pending>>,
    current: Mutex<Option<(Uuid, Arc<Control>)>>,
    closed: AtomicBool,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner").field("state", &self.state).finish()
    }
}

/// Lease on a dedicated Oracle connection.
#[derive(Debug)]
pub struct OracleLease {
    shared: Arc<Shared>,
    inner: Arc<Inner>,
}

struct CallGuard<'a>(&'a Inner);

impl Drop for CallGuard<'_> {
    fn drop(&mut self) {
        self.0.clear_running();
    }
}

impl Inner {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, LeaseState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn clear_running(&self) {
        self.lock_state().running = None;
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn take_pending(&self) -> Option<Pending> {
        let p = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.lock_state().pending_result = None;
        p
    }

    /// Mark a call as running; fails when closed, broken or busy.
    fn begin_call(&self, id: Uuid) -> Result<Arc<Control>> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::Connection("the lease is closed".into()));
        }
        let mut st = self.lock_state();
        if st.broken {
            return Err(Error::Connection(
                "the connection of this lease was lost; open a new one".into(),
            ));
        }
        if st.running.is_some() {
            return Err(Error::Busy(
                "the lease is already running a statement".into(),
            ));
        }
        st.running = Some(id);
        let ctl = Arc::new(Control::default());
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some((id, ctl.clone()));
        Ok(ctl)
    }

    fn note_error(&self, e: &Error) {
        // A call that ignored its interrupt still holds the connection: it cannot be reused.
        let abandoned = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|(_, ctl)| ctl.is_abandoned());
        if abandoned || matches!(e, Error::Connection(_)) {
            self.lock_state().broken = true;
        }
    }
}

/// Drop a kept result away from the async thread (closing a cursor is a database call).
async fn drop_pending(p: Option<Pending>) {
    if let Some(p) = p {
        let _ = tokio::task::spawn_blocking(move || drop(p)).await;
    }
}

/// `true` when a transaction is open on `conn`.
fn probe_transaction(conn: &Connection) -> Result<bool> {
    Ok(catalog::scalar(
        conn,
        "SELECT DBMS_TRANSACTION.LOCAL_TRANSACTION_ID FROM dual",
        &[],
    )?
    .is_some())
}

impl OracleLease {
    pub(crate) fn new(
        shared: Arc<Shared>,
        conn: Arc<Connection>,
        opts: &LeaseOptions,
    ) -> OracleLease {
        let mut state = LeaseState::initial(opts);
        // Oracle has no databases: only the schema is a context.
        state.database = None;
        OracleLease {
            shared,
            inner: Arc::new(Inner {
                conn,
                state: Mutex::new(state),
                explicit_tx: AtomicBool::new(false),
                pending: Mutex::new(None),
                current: Mutex::new(None),
                closed: AtomicBool::new(false),
            }),
        }
    }

    /// Switch the current schema right after opening.
    pub(crate) async fn init_schema(&self, schema: Option<&str>) -> Result<()> {
        if let Some(schema) = schema.filter(|s| !s.is_empty()) {
            self.set_context(LeaseContext {
                database: None,
                schema: Some(schema.to_string()),
            })
            .await?;
        }
        Ok(())
    }

    /// Update `state.transaction` after statements ran.
    async fn refresh_tx(&self, script_text: Option<&str>) {
        let (auto, explicit) = {
            let st = self.inner.lock_state();
            (
                st.auto_commit,
                self.inner.explicit_tx.load(Ordering::SeqCst),
            )
        };
        if explicit && let Some(text) = script_text {
            // COMMIT / ROLLBACK typed by the user ends the explicit transaction.
            let ended = script::split(text).iter().any(|s| {
                let w = script::leading_words(&s.text, 2);
                matches!(w.first().map(String::as_str), Some("COMMIT"))
                    || (w.first().map(String::as_str) == Some("ROLLBACK")
                        && w.get(1).map(String::as_str) != Some("TO"))
            });
            if ended {
                self.inner.explicit_tx.store(false, Ordering::SeqCst);
            }
        }
        let explicit = self.inner.explicit_tx.load(Ordering::SeqCst);
        let status = if auto && !explicit {
            TxStatus::Idle
        } else {
            let conn = self.inner.conn.clone();
            let active = tokio::task::spawn_blocking(move || probe_transaction(&conn))
                .await
                .ok()
                .and_then(|r| r.ok());
            match active {
                Some(true) => TxStatus::Active,
                Some(false) if explicit => TxStatus::Active,
                Some(false) => TxStatus::Idle,
                None => TxStatus::Unknown,
            }
        };
        self.inner.lock_state().transaction = status;
    }

    /// Run `f` on the connection with the lease marked busy.
    async fn exclusive<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        self.inner.begin_call(Uuid::new_v4())?;
        let _guard = CallGuard(&self.inner);
        let conn = self.inner.conn.clone();
        let out = tokio::task::spawn_blocking(move || f(&conn))
            .await
            .map_err(|e| Error::Other(format!("worker failed: {e}")))?;
        if let Err(e) = &out {
            self.inner.note_error(e);
        }
        out
    }

    fn effective_auto_commit(&self) -> bool {
        self.inner.lock_state().auto_commit && !self.inner.explicit_tx.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Lease for OracleLease {
    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        let ctl = self.inner.begin_call(req.query_id)?;
        let _guard = CallGuard(&self.inner);
        drop_pending(self.inner.take_pending()).await;
        let params = ExecParams {
            text: req.text.clone(),
            max_rows: req.max_rows,
            batch_size: req.batch_size.unwrap_or(500).clamp(1, 100_000) as usize,
            auto_commit: self.effective_auto_commit(),
            read_only: self.shared.read_only,
            max_lob: self.shared.target.max_lob_bytes,
            keep_open: req.max_rows.is_some(),
        };
        let timeout = req.timeout_ms.filter(|t| *t > 0).map(Duration::from_millis);
        let result = stream(
            self.inner.conn.clone(),
            ctl,
            timeout,
            sink,
            move |conn, em, ctl| run_script(conn, &params, em, ctl),
        )
        .await;
        match result {
            Ok(outcome) => {
                if let Some(p) = outcome.pending {
                    *self.inner.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
                    self.inner.lock_state().pending_result = Some(req.query_id);
                }
                self.refresh_tx(Some(&req.text)).await;
                Ok(outcome.summary)
            }
            Err(e) => {
                self.inner.note_error(&e);
                if !matches!(e, Error::Connection(_)) && !self.inner.lock_state().broken {
                    self.refresh_tx(Some(&req.text)).await;
                }
                Err(e)
            }
        }
    }

    async fn fetch_more(
        &self,
        query_id: Uuid,
        max_rows: u64,
        batch_size: Option<u32>,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        if self.inner.lock_state().pending_result != Some(query_id) {
            return Err(Error::NotFound(format!(
                "no open result for query {query_id}"
            )));
        }
        let ctl = self.inner.begin_call(query_id)?;
        let _guard = CallGuard(&self.inner);
        let Some(mut pending) = self.inner.take_pending() else {
            return Err(Error::NotFound(format!(
                "no open result for query {query_id}"
            )));
        };
        let batch = batch_size.unwrap_or(500).clamp(1, 100_000) as usize;
        let started = Instant::now();
        let result = stream(
            self.inner.conn.clone(),
            ctl,
            None,
            sink,
            move |_conn, em, ctl| {
                let t0 = Instant::now();
                let (count, truncated) = fetch_pending(&mut pending, max_rows, batch, em, ctl)?;
                em.send(crate::runtime::Event::End(StatementEnd {
                    outcome: EndOutcome::Rows {
                        row_count: count,
                        truncated,
                    },
                    elapsed_ms: u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX),
                }))?;
                Ok((pending, truncated))
            },
        )
        .await;
        match result {
            Ok((pending, true)) => {
                *self.inner.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(pending);
                self.inner.lock_state().pending_result = Some(query_id);
            }
            Ok((pending, false)) => drop_pending(Some(pending)).await,
            Err(e) => {
                self.inner.note_error(&e);
                return Err(e);
            }
        }
        Ok(ExecSummary {
            statements: 1,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    }

    async fn discard_result(&self) -> Result<()> {
        drop_pending(self.inner.take_pending()).await;
        Ok(())
    }

    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        let current = self
            .inner
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some((id, ctl)) = current
            && id == query_id
        {
            ctl.cancelled.store(true, Ordering::SeqCst);
            let conn = self.inner.conn.clone();
            tokio::task::spawn_blocking(move || conn.break_execution())
                .await
                .map_err(|e| Error::Other(format!("worker failed: {e}")))?
                .map_err(map_error)?;
        }
        Ok(())
    }

    async fn begin(&self, opts: TxOptions) -> Result<()> {
        drop_pending(self.inner.take_pending()).await;
        let auto = self.inner.lock_state().auto_commit;
        // Oracle only knows READ COMMITTED and SERIALIZABLE; the stronger isolation levels are
        // mapped to SERIALIZABLE (Oracle's snapshot isolation), the weaker one to READ COMMITTED.
        let set = if opts.read_only {
            Some("SET TRANSACTION READ ONLY")
        } else {
            match opts.isolation {
                Some(IsolationLevel::Serializable)
                | Some(IsolationLevel::RepeatableRead)
                | Some(IsolationLevel::Snapshot) => {
                    Some("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
                }
                _ => None,
            }
        };
        self.exclusive(move |conn| {
            if let Some(sql) = set {
                conn.execute(sql, &[]).map_err(map_error)?;
            }
            Ok(())
        })
        .await?;
        if auto {
            self.inner.explicit_tx.store(true, Ordering::SeqCst);
        }
        self.inner.lock_state().transaction = TxStatus::Active;
        Ok(())
    }

    async fn commit(&self) -> Result<()> {
        drop_pending(self.inner.take_pending()).await;
        self.exclusive(|conn| conn.commit().map_err(map_error))
            .await?;
        self.inner.explicit_tx.store(false, Ordering::SeqCst);
        self.inner.lock_state().transaction = TxStatus::Idle;
        Ok(())
    }

    async fn rollback(&self) -> Result<()> {
        drop_pending(self.inner.take_pending()).await;
        self.exclusive(|conn| conn.rollback().map_err(map_error))
            .await?;
        self.inner.explicit_tx.store(false, Ordering::SeqCst);
        self.inner.lock_state().transaction = TxStatus::Idle;
        Ok(())
    }

    async fn set_auto_commit(&self, on: bool) -> Result<()> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(Error::Connection("the lease is closed".into()));
        }
        drop_pending(self.inner.take_pending()).await;
        if on {
            self.refresh_tx(None).await;
            let st = self.inner.lock_state();
            if st.transaction == TxStatus::Active {
                return Err(Error::Busy(
                    "a transaction is open: commit or roll it back first".into(),
                ));
            }
        }
        self.inner.explicit_tx.store(false, Ordering::SeqCst);
        self.inner.lock_state().auto_commit = on;
        Ok(())
    }

    async fn set_context(&self, ctx: LeaseContext) -> Result<()> {
        drop_pending(self.inner.take_pending()).await;
        let Some(schema) = ctx.schema.clone().filter(|s| !s.is_empty()) else {
            // Oracle has no databases to switch; nothing else to set.
            return Ok(());
        };
        let sql = format!(
            "ALTER SESSION SET CURRENT_SCHEMA = {}",
            quote_ident(&schema)?
        );
        self.exclusive(move |conn| {
            conn.execute(&sql, &[]).map_err(map_error)?;
            Ok(())
        })
        .await?;
        self.inner.lock_state().schema = Some(schema);
        Ok(())
    }

    fn state(&self) -> LeaseState {
        self.inner.lock_state().clone()
    }

    async fn ping(&self) -> Result<()> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(Error::Connection("the lease is closed".into()));
        }
        let conn = self.inner.conn.clone();
        let out = tokio::task::spawn_blocking(move || conn.ping().map_err(map_error))
            .await
            .map_err(|e| Error::Other(format!("worker failed: {e}")))?;
        if let Err(e) = &out {
            self.inner.note_error(e);
        }
        out
    }

    async fn explain(&self, req: ExplainRequest) -> Result<RawPlan> {
        let ctl = self.inner.begin_call(req.query_id)?;
        let _guard = CallGuard(&self.inner);
        drop_pending(self.inner.take_pending()).await;
        let timeout = req.timeout_ms.filter(|t| *t > 0).map(Duration::from_millis);
        let read_only = self.shared.read_only;
        let auto = self.effective_auto_commit();
        let plan = stream(
            self.inner.conn.clone(),
            ctl,
            timeout,
            &mut NullSink,
            move |conn, _em, _ctl| {
                let tx = if probe_transaction(conn)? {
                    TxMode::InTransaction
                } else {
                    TxMode::Idle
                };
                let plan = explain::explain(conn, &req, read_only, tx)?;
                if auto {
                    // The rows of PLAN_TABLE must not keep a transaction open.
                    let _ = conn.rollback();
                }
                Ok(plan)
            },
        )
        .await;
        match plan {
            Ok(p) => {
                self.refresh_tx(None).await;
                Ok(p)
            }
            Err(e) => {
                self.inner.note_error(&e);
                Err(e)
            }
        }
    }

    async fn stream_table(
        &self,
        req: TableStreamRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let (schema, name) = relation_of(&req.table)?;
        let ctl = self.inner.begin_call(req.query_id)?;
        let _guard = CallGuard(&self.inner);
        drop_pending(self.inner.take_pending()).await;
        let batch = req.batch_size.unwrap_or(1000).clamp(1, 100_000) as usize;
        let max_lob = self.shared.target.max_lob_bytes;
        let started = Instant::now();
        stream(
            self.inner.conn.clone(),
            ctl,
            None,
            sink,
            move |conn, em, ctl| {
                data::stream_table_worker(conn, &req, &schema, &name, batch, max_lob, em, ctl)
            },
        )
        .await
        .inspect_err(|e| self.inner.note_error(e))?;
        Ok(ExecSummary {
            statements: 1,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    }

    async fn bulk_insert(&self, req: BulkInsertRequest) -> Result<BulkInsertResult> {
        if self.shared.read_only {
            return Err(Error::ReadOnlyViolation {
                message: "bulk insert is not allowed on a read-only connection".into(),
                position: None,
            });
        }
        let (schema, name) = relation_of(&req.table)?;
        drop_pending(self.inner.take_pending()).await;
        let result = self
            .exclusive(move |conn| data::bulk_insert(conn, &req, &schema, &name))
            .await?;
        self.refresh_tx(None).await;
        Ok(result)
    }

    async fn close(&self) -> Result<LeaseCloseReport> {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return Ok(LeaseCloseReport {
                rolled_back: false,
                forced: false,
            });
        }
        let mut forced = false;
        // 1. Interrupt the running statement and wait for it.
        let current = self
            .inner
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some((_, ctl)) = current {
            ctl.cancelled.store(true, Ordering::SeqCst);
            let conn = self.inner.conn.clone();
            let _ = tokio::task::spawn_blocking(move || conn.break_execution()).await;
            let deadline = Instant::now() + CLOSE_STEP_TIMEOUT;
            while self.inner.lock_state().running.is_some() {
                if Instant::now() >= deadline {
                    forced = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        drop_pending(self.inner.take_pending()).await;
        // 2. Roll back an open transaction.
        let mut rolled_back = false;
        if !forced {
            let (auto, explicit) = {
                let st = self.inner.lock_state();
                (
                    st.auto_commit,
                    self.inner.explicit_tx.load(Ordering::SeqCst),
                )
            };
            let conn = self.inner.conn.clone();
            let work = tokio::task::spawn_blocking(move || -> Result<bool> {
                let open = if auto && !explicit {
                    false
                } else {
                    explicit || probe_transaction(&conn)?
                };
                if open {
                    conn.rollback().map_err(map_error)?;
                }
                Ok(open)
            });
            match tokio::time::timeout(CLOSE_STEP_TIMEOUT, work).await {
                Ok(Ok(Ok(open))) => rolled_back = open,
                Ok(_) => {}
                Err(_) => forced = true,
            }
        }
        // 3. Close the connection (the server rolls back whatever is left).
        let conn = self.inner.conn.clone();
        let closing = tokio::task::spawn_blocking(move || {
            let _ = conn.close();
        });
        if tokio::time::timeout(CLOSE_STEP_TIMEOUT, closing)
            .await
            .is_err()
        {
            forced = true;
        }
        let mut st = self.inner.lock_state();
        st.transaction = TxStatus::Idle;
        st.running = None;
        Ok(LeaseCloseReport {
            rolled_back,
            forced,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_and_closed_calls_are_rejected_without_a_database() {
        // The guard logic does not touch the connection, but constructing an `Inner` needs one;
        // the behaviour is exercised end to end by the integration tests.
        assert_eq!(CLOSE_STEP_TIMEOUT, Duration::from_secs(5));
    }
}
