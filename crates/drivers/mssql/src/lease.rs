//! `Lease`: one dedicated connection for an editor tab.
//!
//! * Statements run in a pump task that owns the connection while a request is in flight and
//!   streams events to the caller through a bounded channel (back pressure). A result set that
//!   reaches `max_rows` pauses the pump (the connection keeps the rest of the rows) until
//!   `fetch_more`, `discard_result`, another call or `close`.
//! * Auto-commit is the connection default; manual mode is `SET IMPLICIT_TRANSACTIONS ON`.
//!   The transaction state comes from `@@TRANCOUNT` / `XACT_STATE()` after every call.
//! * `cancel` sends the TDS attention through the connection's cancel handle (see the vendored
//!   `tiberius`); `close` cancels, rolls back, and closes the socket (forced after 5 s per step).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dbstudio_core::{
    BulkInsertRequest, BulkInsertResult, ColumnMeta, Error, ExecSummary, ExplainRequest,
    IsolationLevel, Lease, LeaseCloseReport, LeaseContext, LeaseOptions, LeaseState, QueryRequest,
    RawPlan, Result, ResultSink, StatementEnd, StatementStart, TableStreamRequest, TxOptions,
    TxStatus, Value,
};
use tiberius::CancelHandle;
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use uuid::Uuid;

use crate::bulk::{self, Reports};
use crate::conn::Conn;
use crate::exec::{Control, Gate, GateMsg, Out, RunParams, run_script};
use crate::session::{DEFAULT_BATCH, MssqlSession};
use crate::sql::{parse_object, quote};
use crate::value::{int_at, str_at};

/// Time each step of closing gets before the socket is dropped.
const CLOSE_STEP: Duration = Duration::from_secs(5);

/// Events of the pump task.
enum Ev {
    Start(StatementStart),
    Columns(Vec<ColumnMeta>),
    Rows(Vec<Vec<Value>>),
    Message(String),
    End(StatementEnd),
    Paused,
}

struct ChanOut(mpsc::Sender<Ev>);

impl ChanOut {
    async fn send(&self, e: Ev) -> Result<()> {
        self.0.send(e).await.map_err(|_| Error::Cancelled)
    }
}

#[async_trait]
impl Out for ChanOut {
    async fn start(&mut self, s: StatementStart) -> Result<()> {
        self.send(Ev::Start(s)).await
    }
    async fn columns(&mut self, c: Vec<ColumnMeta>) -> Result<()> {
        self.send(Ev::Columns(c)).await
    }
    async fn rows(&mut self, r: Vec<Vec<Value>>) -> Result<()> {
        self.send(Ev::Rows(r)).await
    }
    async fn message(&mut self, m: String) -> Result<()> {
        self.send(Ev::Message(m)).await
    }
    async fn end(&mut self, e: StatementEnd) -> Result<()> {
        self.send(Ev::End(e)).await
    }
    async fn paused(&mut self) -> Result<()> {
        self.send(Ev::Paused).await
    }
}

struct Pending {
    id: Uuid,
    task: JoinHandle<(Conn, Result<ExecSummary>)>,
    events: mpsc::Receiver<Ev>,
    gate: mpsc::Sender<GateMsg>,
    ctl: Arc<Control>,
}

struct Inner {
    conn: Option<Conn>,
    pending: Option<Pending>,
}

struct Shared {
    state: Mutex<LeaseState>,
    /// Statement being executed.
    running: Mutex<Option<(Uuid, Arc<Control>)>>,
    abort: Mutex<Option<AbortHandle>>,
    handle: CancelHandle,
    closed: AtomicBool,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut LeaseState)) {
        if let Ok(mut s) = self.state.lock() {
            f(&mut s);
        }
    }
}

/// A dedicated connection.
pub struct MssqlLease {
    shared: Arc<Shared>,
    inner: tokio::sync::Mutex<Inner>,
}

impl std::fmt::Debug for MssqlLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MssqlLease")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

/// Open a lease on `sess`.
pub async fn open(sess: &MssqlSession, opts: LeaseOptions) -> Result<Box<dyn Lease>> {
    let db = opts
        .database
        .clone()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| sess.login_db().to_string());
    let mut conn = Conn::open(&sess.shared.settings, &db).await?;
    if !opts.auto_commit {
        conn.run("SET IMPLICIT_TRANSACTIONS ON").await?;
    }
    let mut state = LeaseState::initial(&opts);
    state.database = Some(db);
    let shared = Arc::new(Shared {
        state: Mutex::new(state),
        running: Mutex::new(None),
        abort: Mutex::new(None),
        handle: conn.cancel.clone(),
        closed: AtomicBool::new(false),
    });
    Ok(Box::new(MssqlLease {
        shared,
        inner: tokio::sync::Mutex::new(Inner {
            conn: Some(conn),
            pending: None,
        }),
    }))
}

fn forward(ev: Ev, sink: &mut dyn ResultSink, statements: &mut u32) -> Result<()> {
    match ev {
        Ev::Start(s) => sink.on_statement_start(s),
        Ev::Columns(c) => sink.on_columns(c),
        Ev::Rows(r) => sink.on_rows(r),
        Ev::Message(m) => sink.on_message(m),
        Ev::End(e) => {
            *statements += 1;
            sink.on_statement_end(e)
        }
        Ev::Paused => Ok(()),
    }
}

impl MssqlLease {
    fn check_open(&self) -> Result<()> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(Error::Connection("the lease is closed".into()));
        }
        if self.shared.state.lock().map(|s| s.broken).unwrap_or(true) {
            return Err(Error::Connection(
                "the connection of this lease was lost; open a new lease".into(),
            ));
        }
        Ok(())
    }

    fn busy() -> Error {
        Error::Busy("the lease is running a statement".into())
    }

    /// Stop a paused result set and take the connection back.
    async fn discard_pending(&self, inner: &mut Inner) {
        let Some(p) = inner.pending.take() else {
            return;
        };
        let _ = p.gate.send(GateMsg::Discard).await;
        let _ = self.finish(inner, p).await;
        self.shared.update(|s| s.pending_result = None);
    }

    /// Wait for the pump to end and take its connection (dropping any remaining events).
    async fn finish(&self, inner: &mut Inner, mut p: Pending) -> Result<ExecSummary> {
        while p.events.recv().await.is_some() {}
        drop(p.gate);
        match p.task.await {
            Ok((conn, result)) => {
                if conn.broken {
                    self.shared.update(|s| s.broken = true);
                }
                inner.conn = Some(conn);
                result
            }
            Err(_) => {
                self.shared.update(|s| s.broken = true);
                Err(Error::Connection(
                    "the connection of this lease was lost".into(),
                ))
            }
        }
    }

    /// Start `text` on the connection and drive it until it ends or pauses.
    async fn start_run(
        &self,
        inner: &mut Inner,
        id: Uuid,
        text: String,
        params: RunParams,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let Some(mut conn) = inner.conn.take() else {
            return Err(Error::Connection("the lease has no connection".into()));
        };
        let ctl = Arc::new(Control::default());
        let (tx, rx) = mpsc::channel::<Ev>(32);
        let (gate_tx, gate_rx) = mpsc::channel::<GateMsg>(1);
        let task_ctl = ctl.clone();
        let task = tokio::spawn(async move {
            let mut gate = Gate { rx: gate_rx };
            let mut out = ChanOut(tx);
            let r = run_script(
                &mut conn,
                &text,
                &params,
                &task_ctl,
                Some(&mut gate),
                &mut out,
            )
            .await;
            (conn, r)
        });
        if let Ok(mut a) = self.shared.abort.lock() {
            *a = Some(task.abort_handle());
        }
        if let Ok(mut r) = self.shared.running.lock() {
            *r = Some((id, ctl.clone()));
        }
        self.shared.update(|s| {
            s.running = Some(id);
            s.pending_result = None;
        });
        let pending = Pending {
            id,
            task,
            events: rx,
            gate: gate_tx,
            ctl,
        };
        self.drive(inner, pending, sink).await
    }

    /// Forward the events of `p` to `sink` until the run ends or pauses.
    async fn drive(
        &self,
        inner: &mut Inner,
        mut p: Pending,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let started = Instant::now();
        let mut statements = 0u32;
        let mut sink_error: Option<Error> = None;
        loop {
            match p.events.recv().await {
                Some(Ev::Paused) => {
                    let id = p.id;
                    inner.pending = Some(p);
                    self.shared.update(|s| {
                        s.running = None;
                        s.pending_result = Some(id);
                    });
                    if let Ok(mut r) = self.shared.running.lock() {
                        *r = None;
                    }
                    return match sink_error {
                        Some(e) => Err(e),
                        None => Ok(ExecSummary {
                            statements,
                            elapsed_ms: started.elapsed().as_millis() as u64,
                        }),
                    };
                }
                Some(ev) => {
                    if sink_error.is_none() {
                        if let Err(e) = forward(ev, sink, &mut statements) {
                            sink_error = Some(e);
                            // Ask the server to stop; the pump discards the rest.
                            p.ctl.cancel();
                            self.shared.handle.cancel();
                        }
                    }
                }
                None => break,
            }
        }
        let result = self.finish(inner, p).await;
        if let Ok(mut r) = self.shared.running.lock() {
            *r = None;
        }
        self.shared.update(|s| s.running = None);
        self.refresh_tx(inner).await;
        if let Some(e) = sink_error {
            return Err(e);
        }
        result
    }

    /// Read the transaction state from the server.
    async fn refresh_tx(&self, inner: &mut Inner) {
        let Some(conn) = inner.conn.as_mut() else {
            return;
        };
        if conn.broken {
            self.shared.update(|s| {
                s.broken = true;
                s.transaction = TxStatus::Unknown;
            });
            return;
        }
        // A cancel that lands on the probe itself ends it with an attention: ask again.
        let mut probe = None;
        for _ in 0..2 {
            match tokio::time::timeout(
                CLOSE_STEP,
                conn.rows("SELECT @@TRANCOUNT, XACT_STATE(), DB_NAME(), @@OPTIONS & 2"),
            )
            .await
            {
                Ok(Ok(rows)) => {
                    probe = Some(rows);
                    break;
                }
                Ok(Err(Error::Cancelled)) => continue,
                _ => {
                    conn.broken = true;
                    break;
                }
            }
        }
        match probe.as_ref().and_then(|r| r.first().cloned()) {
            Some(r) => {
                let status = if int_at(&r, 1) == -1 {
                    TxStatus::Failed
                } else if int_at(&r, 0) > 0 {
                    TxStatus::Active
                } else {
                    TxStatus::Idle
                };
                let db = str_at(&r, 2);
                let implicit = int_at(&r, 3) != 0;
                self.shared.update(|s| {
                    s.transaction = status;
                    s.auto_commit = !implicit;
                    if !db.is_empty() {
                        s.database = Some(db);
                    }
                });
            }
            None if conn.broken => {
                self.shared.update(|s| {
                    s.broken = true;
                    s.transaction = TxStatus::Unknown;
                });
            }
            // Still cancelled: the state is unknown but the connection is fine.
            None => self.shared.update(|s| s.transaction = TxStatus::Unknown),
        }
    }

    /// Take the connection for a short control statement (nothing pending, nothing running).
    async fn control(&self, sql: &str) -> Result<()> {
        self.check_open()?;
        let mut inner = self.inner.try_lock().map_err(|_| Self::busy())?;
        self.discard_pending(&mut inner).await;
        let conn = inner
            .conn
            .as_mut()
            .ok_or_else(|| Error::Connection("the lease has no connection".into()))?;
        let r = conn.run(sql).await;
        let broken = conn.broken;
        if broken {
            self.shared.update(|s| s.broken = true);
        }
        self.refresh_tx(&mut inner).await;
        r
    }

    fn params(req: &QueryRequest) -> RunParams {
        MssqlSession::run_params(req)
    }

    /// Conn of an idle lease, after dropping a paused result.
    async fn idle<'a>(
        &self,
        inner: &'a mut tokio::sync::MutexGuard<'_, Inner>,
    ) -> Result<&'a mut Conn> {
        self.discard_pending(inner).await;
        inner
            .conn
            .as_mut()
            .ok_or_else(|| Error::Connection("the lease has no connection".into()))
    }
}

fn isolation_sql(level: IsolationLevel) -> &'static str {
    match level {
        IsolationLevel::ReadUncommitted => "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
        IsolationLevel::ReadCommitted => "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
        IsolationLevel::RepeatableRead => "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        IsolationLevel::Serializable => "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        IsolationLevel::Snapshot => "SET TRANSACTION ISOLATION LEVEL SNAPSHOT",
    }
}

#[async_trait]
impl Lease for MssqlLease {
    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        self.check_open()?;
        let mut inner = self.inner.try_lock().map_err(|_| Self::busy())?;
        self.idle(&mut inner).await?;
        let params = Self::params(&req);
        self.start_run(&mut inner, req.query_id, req.text, params, sink)
            .await
    }

    async fn fetch_more(
        &self,
        query_id: Uuid,
        max_rows: u64,
        batch_size: Option<u32>,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        self.check_open()?;
        let mut inner = self.inner.try_lock().map_err(|_| Self::busy())?;
        let p = match inner.pending.take() {
            Some(p) if p.id == query_id => p,
            other => {
                inner.pending = other;
                return Err(Error::NotFound(format!(
                    "no pending result for query {query_id}"
                )));
            }
        };
        let batch = batch_size.map(|b| (b as usize).clamp(1, 100_000));
        if p.gate
            .send(GateMsg::Continue {
                rows: max_rows.max(1),
                batch,
            })
            .await
            .is_err()
        {
            self.finish(&mut inner, p).await?;
            return Err(Error::NotFound("the pending result ended".into()));
        }
        if let Ok(mut r) = self.shared.running.lock() {
            *r = Some((p.id, p.ctl.clone()));
        }
        self.shared.update(|s| {
            s.running = Some(query_id);
            s.pending_result = None;
        });
        self.drive(&mut inner, p, sink).await
    }

    async fn discard_result(&self) -> Result<()> {
        self.check_open()?;
        let mut inner = self.inner.try_lock().map_err(|_| Self::busy())?;
        self.discard_pending(&mut inner).await;
        self.refresh_tx(&mut inner).await;
        Ok(())
    }

    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        let target = self.shared.running.lock().ok().and_then(|r| {
            r.as_ref()
                .filter(|(id, _)| *id == query_id)
                .map(|(_, c)| c.clone())
        });
        if let Some(ctl) = target {
            ctl.cancel();
            self.shared.handle.cancel();
        }
        Ok(())
    }

    async fn begin(&self, opts: TxOptions) -> Result<()> {
        // `read_only` has no T-SQL equivalent; the application guard enforces it.
        let mut sql = String::new();
        if let Some(level) = opts.isolation {
            sql.push_str(isolation_sql(level));
            sql.push_str(";\n");
        }
        // Inside an implicit transaction one is already open: starting another would nest.
        sql.push_str("IF @@TRANCOUNT = 0 BEGIN TRANSACTION");
        self.control(&sql).await
    }

    async fn commit(&self) -> Result<()> {
        self.control("WHILE @@TRANCOUNT > 0 COMMIT TRANSACTION")
            .await
    }

    async fn rollback(&self) -> Result<()> {
        self.control("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION")
            .await
    }

    async fn set_auto_commit(&self, on: bool) -> Result<()> {
        self.check_open()?;
        {
            let mut inner = self.inner.try_lock().map_err(|_| Self::busy())?;
            self.discard_pending(&mut inner).await;
            self.refresh_tx(&mut inner).await;
            let active = self
                .shared
                .state
                .lock()
                .map(|s| s.transaction != TxStatus::Idle)
                .unwrap_or(false);
            if on && active {
                return Err(Error::Busy(
                    "a transaction is open: commit or roll it back first".into(),
                ));
            }
        }
        self.control(if on {
            "SET IMPLICIT_TRANSACTIONS OFF"
        } else {
            "SET IMPLICIT_TRANSACTIONS ON"
        })
        .await?;
        self.shared.update(|s| s.auto_commit = on);
        Ok(())
    }

    async fn set_context(&self, ctx: LeaseContext) -> Result<()> {
        if let Some(db) = ctx.database.as_deref().filter(|d| !d.is_empty()) {
            self.control(&format!("USE {}", quote(db))).await?;
            self.shared.update(|s| s.database = Some(db.to_string()));
        }
        // SQL Server has no session schema; the value is kept for the editor only.
        if ctx.schema.is_some() {
            self.shared.update(|s| s.schema = ctx.schema.clone());
        }
        Ok(())
    }

    fn state(&self) -> LeaseState {
        self.shared
            .state
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }

    async fn ping(&self) -> Result<()> {
        self.check_open()?;
        let mut inner = self.inner.try_lock().map_err(|_| Self::busy())?;
        let conn = self.idle(&mut inner).await?;
        let r = conn.rows("SELECT 1").await.map(|_| ());
        if conn.broken || r.is_err() {
            self.shared.update(|s| s.broken = true);
        }
        r
    }

    async fn explain(&self, req: ExplainRequest) -> Result<RawPlan> {
        self.check_open()?;
        let mut inner = self.inner.try_lock().map_err(|_| Self::busy())?;
        let conn = self.idle(&mut inner).await?;
        let ctl = Arc::new(Control::default());
        if let Ok(mut r) = self.shared.running.lock() {
            *r = Some((req.query_id, ctl.clone()));
        }
        self.shared.update(|s| s.running = Some(req.query_id));
        let shared = self.shared.clone();
        let mut done = move || {
            if let Ok(mut r) = shared.running.lock() {
                *r = None;
            }
            shared.update(|s| s.running = None);
        };
        let plan = crate::explain::explain_on(conn, &ctl, &req, &mut done).await;
        done();
        self.refresh_tx(&mut inner).await;
        plan
    }

    async fn stream_table(
        &self,
        req: TableStreamRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        self.check_open()?;
        let obj = parse_object(&req.table)?;
        let mut inner = self.inner.try_lock().map_err(|_| Self::busy())?;
        let conn = self.idle(&mut inner).await?;
        let info = crate::data::load_table_info(conn, &obj).await;
        let sql = crate::data::stream_sql(&obj, &req, &info, conn.lob_limit)?;
        let params = RunParams {
            max_rows: None,
            batch_size: req
                .batch_size
                .map(|b| b as usize)
                .unwrap_or(DEFAULT_BATCH)
                .clamp(1, 100_000),
            timeout: None,
        };
        self.start_run(&mut inner, req.query_id, sql, params, sink)
            .await
    }

    async fn bulk_insert(&self, req: BulkInsertRequest) -> Result<BulkInsertResult> {
        self.check_open()?;
        let obj = parse_object(&req.table)?;
        let mut inner = self.inner.try_lock().map_err(|_| Self::busy())?;
        let conn = self.idle(&mut inner).await?;
        let r = bulk_on(conn, &obj, &req).await;
        self.refresh_tx(&mut inner).await;
        r
    }

    async fn close(&self) -> Result<LeaseCloseReport> {
        if self.shared.closed.swap(true, Ordering::AcqRel) {
            return Ok(LeaseCloseReport {
                rolled_back: false,
                forced: false,
            });
        }
        let mut forced = false;
        // Stop what is running and wait for the call that drives it.
        let running = self
            .shared
            .running
            .lock()
            .ok()
            .and_then(|r| r.as_ref().map(|(_, c)| c.clone()));
        if let Some(ctl) = running {
            ctl.cancel();
            self.shared.handle.cancel();
        }
        let mut inner = match tokio::time::timeout(CLOSE_STEP, self.inner.lock()).await {
            Ok(g) => g,
            Err(_) => {
                // The statement does not stop: dropping the pump task closes the socket.
                forced = true;
                if let Ok(a) = self.shared.abort.lock() {
                    if let Some(a) = a.as_ref() {
                        a.abort();
                    }
                }
                self.inner.lock().await
            }
        };
        if tokio::time::timeout(CLOSE_STEP, self.discard_pending(&mut inner))
            .await
            .is_err()
        {
            forced = true;
        }
        let mut rolled_back = false;
        if let Some(mut conn) = inner.conn.take() {
            if !conn.broken && !forced {
                let tx = tokio::time::timeout(CLOSE_STEP, conn.rows("SELECT @@TRANCOUNT")).await;
                match tx {
                    Ok(Ok(rows)) => {
                        if rows.first().is_some_and(|r| int_at(r, 0) > 0) {
                            rolled_back = true;
                            if tokio::time::timeout(
                                CLOSE_STEP,
                                conn.run("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION"),
                            )
                            .await
                            .is_err()
                            {
                                forced = true;
                            }
                        }
                    }
                    _ => forced = true,
                }
            } else if forced {
                // A transaction may have been open; the server rolls it back with the socket.
                rolled_back = self
                    .shared
                    .state
                    .lock()
                    .map(|s| s.transaction == TxStatus::Active)
                    .unwrap_or(false);
            }
            let _ = tokio::time::timeout(CLOSE_STEP, conn.client.close()).await;
        }
        self.shared.update(|s| {
            s.transaction = TxStatus::Idle;
            s.running = None;
            s.pending_result = None;
        });
        Ok(LeaseCloseReport {
            rolled_back,
            forced,
        })
    }
}

/// The server doomed the transaction (`XACT_STATE() = -1`): nothing but a rollback is possible.
/// Roll everything back and tell the user; the lease itself stays usable.
async fn doomed(conn: &mut Conn, cause: &str) -> Error {
    if conn
        .run("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION")
        .await
        .is_err()
    {
        conn.broken = true;
    }
    Error::Other(if cause.is_empty() {
        "the transaction was aborted by the server and has been rolled back".to_string()
    } else {
        format!("the transaction was aborted by the server and has been rolled back ({cause})")
    })
}

/// Run the statements of a bulk insert on `conn`.
async fn bulk_on(
    conn: &mut Conn,
    obj: &crate::sql::ObjRef,
    req: &BulkInsertRequest,
) -> Result<BulkInsertResult> {
    // Column types (for casting NULL), key and identity column.
    let filter = crate::catalog::one(&obj.schema, &obj.name);
    let key = (obj.schema.clone(), obj.name.clone());
    let cols = crate::catalog::columns(conn, &filter)
        .await?
        .remove(&key)
        .unwrap_or_default();
    if cols.is_empty() {
        return Err(Error::NotFound(format!("table {}", obj.qualified())));
    }
    let types: Vec<String> = req
        .columns
        .iter()
        .map(|c| {
            cols.iter()
                .find(|x| x.name.eq_ignore_ascii_case(c))
                .map(|x| x.ddl_type.clone())
                .unwrap_or_else(|| "NVARCHAR(MAX)".to_string())
        })
        .collect();
    for c in &req.columns {
        if !cols.iter().any(|x| x.name.eq_ignore_ascii_case(c)) {
            return Err(Error::Query {
                message: format!("Invalid column name '{c}'."),
                position: None,
            });
        }
    }
    let identity = cols.iter().any(|c| {
        c.identity.is_some() && req.columns.iter().any(|r| r.eq_ignore_ascii_case(&c.name))
    });
    let key_columns = if req.on_conflict == dbstudio_core::OnConflict::Error {
        Vec::new()
    } else if req.conflict_columns.is_empty() {
        crate::data::key_columns(conn, obj).await?
    } else {
        req.conflict_columns.clone()
    };
    let stmts = bulk::statements(obj, req, &types, &key_columns)?;
    // Atomic per call even without an open transaction.
    let tx = conn.rows("SELECT @@TRANCOUNT, XACT_STATE()").await?;
    let outer = tx.first().is_some_and(|r| int_at(r, 0) > 0);
    if outer && tx.first().is_some_and(|r| int_at(r, 1) == -1) {
        return Err(doomed(conn, "").await);
    }
    if !outer {
        conn.run("BEGIN TRANSACTION").await?;
    } else {
        conn.run("SAVE TRANSACTION dbstudio_bulk").await?;
    }
    let table = obj.full();
    let result = async {
        if identity {
            conn.run(&format!("SET IDENTITY_INSERT {table} ON")).await?;
        }
        let mut total = BulkInsertResult::default();
        let ctl = Control::default();
        let params = RunParams {
            max_rows: None,
            batch_size: 1000,
            timeout: None,
        };
        for (sql, reports) in &stmts {
            let mut sink = dbstudio_core::CollectingSink::new();
            run_script(
                conn,
                sql,
                &params,
                &ctl,
                None,
                &mut crate::exec::SinkOut(&mut sink),
            )
            .await?;
            let results = sink.into_results();
            match reports {
                Reports::Inserted { rows } => {
                    let inserted = results
                        .iter()
                        .rev()
                        .find_map(|r| match r.outcome {
                            dbstudio_core::StatementOutcome::Affected { rows } => Some(rows),
                            _ => None,
                        })
                        .unwrap_or(0);
                    total.inserted += inserted;
                    total.skipped += (*rows as u64).saturating_sub(inserted);
                }
                Reports::Actions { .. } => {
                    for r in &results {
                        if let dbstudio_core::StatementOutcome::Rows(rs) = &r.outcome {
                            for row in &rs.rows {
                                match str_at(row, 0).as_str() {
                                    "INSERT" => total.inserted += 1,
                                    "UPDATE" => total.updated += 1,
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok::<_, Error>(total)
    }
    .await;
    if identity {
        // Best effort: the setting is per session and per table.
        let _ = conn.run(&format!("SET IDENTITY_INSERT {table} OFF")).await;
    }
    match result {
        Ok(total) => {
            if !outer {
                conn.run("COMMIT TRANSACTION").await?;
            }
            Ok(total)
        }
        Err(e) => {
            if outer {
                // Doomed (`XACT_STATE() = -1`) or already rolled back (`XACT_ABORT`).
                let gone = conn
                    .rows("SELECT @@TRANCOUNT, XACT_STATE()")
                    .await
                    .ok()
                    .and_then(|r| r.first().cloned())
                    .is_some_and(|r| int_at(&r, 1) == -1 || int_at(&r, 0) == 0);
                if gone {
                    return Err(doomed(conn, &e.to_string()).await);
                }
            }
            let undo = if outer {
                "ROLLBACK TRANSACTION dbstudio_bulk"
            } else {
                "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION"
            };
            if conn.run(undo).await.is_err() {
                conn.broken = true;
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_statements() {
        assert_eq!(
            isolation_sql(IsolationLevel::Snapshot),
            "SET TRANSACTION ISOLATION LEVEL SNAPSHOT"
        );
        assert_eq!(
            isolation_sql(IsolationLevel::ReadUncommitted),
            "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED"
        );
    }
}
