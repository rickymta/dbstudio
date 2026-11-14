//! Helper for driver authors: an actor-based [`Lease`] implementation.
//!
//! [`ActorLease`] spawns one tokio task that owns the connection (a [`LeaseBackend`]) and runs
//! the commands it receives over an `mpsc` channel, one at a time. The handle implements
//! [`Lease`]: it answers `Error::Busy` to concurrent calls, matches `query_id` for `cancel` and
//! `fetch_more`, keeps a cached [`LeaseState`] and orders `close` (cancel, rollback, drop).
//!
//! # Events
//!
//! A [`ResultSink`] is borrowed by the caller and cannot move to the actor task, so the actor
//! writes into a [`ChannelSink`] and the caller forwards the events to its own sink. A sink that
//! returns an error makes the channel sink refuse further events (the backend then stops) and
//! the running statement is cancelled; the sink's error is what the caller gets back.
//!
//! # Pending results
//!
//! `execute(.., keep_open = true)` and `fetch_more` may leave the last result set open when it
//! hit `max_rows`. The actor detects this from the events: the last `on_statement_end` is a
//! `Rows { truncated: true }`. A pending result is dropped by `discard_result`, by the next
//! command, by `close`, or automatically after [`LeaseTuning::pending_ttl`] (ten minutes by
//! default, design risk R4) so an abandoned tab does not hold server snapshots forever.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    BulkInsertRequest, BulkInsertResult, ColumnMeta, EndOutcome, Error, ExecSummary,
    ExplainRequest, Lease, LeaseCloseReport, LeaseContext, LeaseOptions, LeaseState, QueryRequest,
    RawPlan, Result, ResultSink, StatementEnd, StatementStart, TableStreamRequest, TxControl,
    TxOptions, TxStatus, Value,
};

/// Default batch size passed to [`LeaseBackend::fetch_more`] when the caller gives none.
pub const DEFAULT_FETCH_BATCH: usize = 1000;

/// What a driver implements to get a [`Lease`] from [`ActorLease`]. All methods run on the actor
/// task that owns the connection, one at a time.
#[async_trait]
pub trait LeaseBackend: Send + 'static {
    /// Run the request. With `keep_open`, a result set that hits `max_rows` stays open for
    /// [`LeaseBackend::fetch_more`]; the backend signals it by ending that statement with
    /// `Rows { truncated: true }`. Without `keep_open` the rest of such a set is discarded.
    async fn execute(
        &mut self,
        req: QueryRequest,
        sink: &mut dyn ResultSink,
        keep_open: bool,
    ) -> Result<ExecSummary>;
    /// Continue the pending result set (`NotFound` when there is none). Ends the statement with
    /// `row_count` of this call and `truncated` = more rows remain.
    async fn fetch_more(
        &mut self,
        max_rows: u64,
        batch: usize,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary>;
    /// Drop the pending result set (a no-op without one).
    async fn discard_result(&mut self) -> Result<()>;
    /// Transaction control. [`ActorLease`] only issues `Begin`, `Commit` and `Rollback`.
    async fn tx(&mut self, op: TxControl, opts: TxOptions) -> Result<()>;
    /// Switch auto-commit.
    async fn set_auto_commit(&mut self, on: bool) -> Result<()>;
    /// Switch database / schema.
    async fn set_context(&mut self, ctx: LeaseContext) -> Result<()>;
    /// Transaction state as the backend knows it (cheap probes or its own bookkeeping).
    async fn probe_tx(&mut self) -> Result<TxStatus>;
    /// Called after every `execute` with the text that ran: report the current database /
    /// schema when the script may have changed it (`USE`, `SET search_path`, ...). Must be cheap
    /// for scripts that cannot have changed anything.
    async fn refresh_context(&mut self, executed_text: &str) -> Result<Option<LeaseContext>> {
        let _ = executed_text;
        Ok(None)
    }
    /// SQL text of the `SELECT` that reads a whole table (export). [`ActorLease`] runs it like
    /// any statement on this connection, so rows stream inside the lease's transaction.
    /// Default: `Unsupported("table_stream")`.
    async fn table_select(&mut self, req: &TableStreamRequest) -> Result<String> {
        let _ = req;
        Err(Error::Unsupported("table_stream".into()))
    }
    /// Insert rows on this very connection, inside its current transaction (no commit).
    /// Default: `Unsupported("bulk_insert")`.
    async fn bulk_insert(&mut self, req: BulkInsertRequest) -> Result<BulkInsertResult> {
        let _ = req;
        Err(Error::Unsupported("bulk_insert".into()))
    }
    /// Explain one statement on this connection, inside its transaction (`ANALYZE` of a write
    /// must leave the transaction's earlier work intact). Default: `Unsupported("explain")`.
    async fn explain(&mut self, req: ExplainRequest) -> Result<RawPlan> {
        let _ = req;
        Err(Error::Unsupported("explain".into()))
    }
    /// Liveness check.
    async fn ping(&mut self) -> Result<()>;
    /// Roll back an open transaction and close the connection; returns whether a transaction
    /// was rolled back.
    async fn close(&mut self) -> Result<bool>;
    /// Handle usable concurrently from another task to interrupt the running call.
    fn canceller(&self) -> Arc<dyn Canceller>;
    /// Whether a result is pending after `execute` / `fetch_more` returned. `None` (default)
    /// lets [`ActorLease`] infer it from the events (last statement ended truncated with
    /// `keep_open`); backends that do not keep every truncated result open must answer.
    fn has_pending(&self) -> Option<bool> {
        None
    }
    /// How long the pending result may wait for `fetch_more` (at most
    /// [`LeaseTuning::pending_ttl`]); shorter when it holds server resources that matter, such
    /// as the transaction of an auto-commit cursor.
    fn pending_ttl(&self) -> Option<Duration> {
        None
    }
}

/// Interrupts the call currently running on a [`LeaseBackend`].
#[async_trait]
pub trait Canceller: Send + Sync {
    /// Interrupt the running statement (protocol-level cancel).
    async fn cancel(&self) -> Result<()>;
    /// Interrupt the running statement only when it belongs to request `query_id`; a request
    /// that already ended (and was replaced by another) must not be hit. Backends that know
    /// the id of their running request override this; the default cancels whatever runs.
    async fn cancel_for(&self, query_id: Uuid) -> Result<()> {
        let _ = query_id;
        self.cancel().await
    }
}

/// Timings of an [`ActorLease`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseTuning {
    /// A pending result nobody fetched for this long is discarded.
    pub pending_ttl: Duration,
    /// `close` waits this long for the running statement to stop after cancelling it.
    pub cancel_wait: Duration,
    /// `close` waits this long for the rollback / graceful close.
    pub rollback_wait: Duration,
    /// A lease that received no command for this long is rolled back and closed (an editor tab
    /// that was forgotten or orphaned). `None` disables the limit.
    pub idle_ttl: Option<Duration>,
    /// After a timeout the statement is cancelled; when it still has not ended after this long
    /// the lease is declared broken and its connection dropped.
    pub timeout_grace: Duration,
}

impl Default for LeaseTuning {
    fn default() -> Self {
        LeaseTuning {
            pending_ttl: Duration::from_secs(10 * 60),
            cancel_wait: Duration::from_secs(5),
            rollback_wait: Duration::from_secs(5),
            idle_ttl: Some(Duration::from_secs(8 * 3600)),
            timeout_grace: Duration::from_secs(10),
        }
    }
}

/// One event of a running statement, as produced by a [`ChannelSink`].
#[derive(Debug)]
pub enum LeaseEvent {
    /// `on_columns`
    Columns(Vec<ColumnMeta>),
    /// `on_rows`
    Rows(Vec<Vec<Value>>),
    /// `on_message`
    Message(String),
    /// `on_statement_end`
    End(StatementEnd),
    /// `on_statement_start`
    Start(StatementStart),
}

impl LeaseEvent {
    /// Hand the event to `sink`.
    pub fn deliver(self, sink: &mut dyn ResultSink) -> Result<()> {
        match self {
            LeaseEvent::Columns(c) => sink.on_columns(c),
            LeaseEvent::Rows(r) => sink.on_rows(r),
            LeaseEvent::Message(m) => sink.on_message(m),
            LeaseEvent::End(e) => sink.on_statement_end(e),
            LeaseEvent::Start(s) => sink.on_statement_start(s),
        }
    }
}

/// A [`ResultSink`] that sends its events over a channel (to be delivered to the real sink by
/// another task). Refuses events with `Error::Cancelled` once the receiver is gone or the stop
/// flag was raised.
#[derive(Debug)]
pub struct ChannelSink {
    tx: mpsc::UnboundedSender<LeaseEvent>,
    stop: Arc<AtomicBool>,
    last_end: Option<EndOutcome>,
}

impl ChannelSink {
    /// Sink sending to `tx`; raising `stop` makes every later call fail.
    pub fn new(tx: mpsc::UnboundedSender<LeaseEvent>, stop: Arc<AtomicBool>) -> Self {
        ChannelSink {
            tx,
            stop,
            last_end: None,
        }
    }

    /// Send events to another channel from now on.
    pub fn retarget(&mut self, tx: mpsc::UnboundedSender<LeaseEvent>, stop: Arc<AtomicBool>) {
        self.tx = tx;
        self.stop = stop;
        self.last_end = None;
    }

    /// Outcome of the most recent `on_statement_end`.
    pub fn last_end(&self) -> Option<&EndOutcome> {
        self.last_end.as_ref()
    }

    /// Whether the last statement ended with a truncated result set.
    pub fn ended_truncated(&self) -> bool {
        matches!(
            self.last_end,
            Some(EndOutcome::Rows {
                truncated: true,
                ..
            })
        )
    }

    fn send(&mut self, event: LeaseEvent) -> Result<()> {
        if self.stop.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        self.tx.send(event).map_err(|_| Error::Cancelled)
    }
}

impl ResultSink for ChannelSink {
    fn on_columns(&mut self, columns: Vec<ColumnMeta>) -> Result<()> {
        self.send(LeaseEvent::Columns(columns))
    }
    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        self.send(LeaseEvent::Rows(rows))
    }
    fn on_message(&mut self, message: String) -> Result<()> {
        self.send(LeaseEvent::Message(message))
    }
    fn on_statement_end(&mut self, end: StatementEnd) -> Result<()> {
        self.last_end = Some(end.outcome.clone());
        self.send(LeaseEvent::End(end))
    }
    fn on_statement_start(&mut self, start: StatementStart) -> Result<()> {
        self.send(LeaseEvent::Start(start))
    }
}

/// Commands the actor runs.
enum Cmd {
    Execute {
        req: QueryRequest,
        keep_open: bool,
        events: mpsc::UnboundedSender<LeaseEvent>,
        stop: Arc<AtomicBool>,
        done: oneshot::Sender<Result<ExecSummary>>,
    },
    Fetch {
        max_rows: u64,
        batch: usize,
        events: mpsc::UnboundedSender<LeaseEvent>,
        stop: Arc<AtomicBool>,
        done: oneshot::Sender<Result<ExecSummary>>,
    },
    Discard(oneshot::Sender<Result<()>>),
    Tx {
        op: TxControl,
        opts: TxOptions,
        reply: oneshot::Sender<Result<()>>,
    },
    AutoCommit {
        on: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    Context {
        ctx: LeaseContext,
        reply: oneshot::Sender<Result<()>>,
    },
    Ping(oneshot::Sender<Result<()>>),
    TableSelect {
        req: TableStreamRequest,
        reply: oneshot::Sender<Result<String>>,
    },
    Bulk {
        req: BulkInsertRequest,
        reply: oneshot::Sender<Result<BulkInsertResult>>,
    },
    Explain {
        req: ExplainRequest,
        reply: oneshot::Sender<Result<RawPlan>>,
    },
    Close(oneshot::Sender<LeaseCloseReport>),
}

#[derive(Debug)]
struct Inner {
    state: LeaseState,
    /// A command is queued or running (set by the caller, cleared by the actor).
    busy: bool,
    closed: bool,
}

#[derive(Debug)]
struct Shared {
    inner: Mutex<Inner>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Claim of the "one command at a time" slot; clears it again when dropped before the command
/// reached the actor.
struct Slot<'a> {
    shared: &'a Shared,
    armed: bool,
}

impl Slot<'_> {
    fn sent(mut self) {
        self.armed = false;
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut g = self.shared.lock();
            g.busy = false;
            g.state.running = None;
        }
    }
}

fn closed_error() -> Error {
    Error::Connection("the lease is closed".into())
}

fn actor_gone() -> Error {
    Error::Connection("the lease connection task stopped".into())
}

fn mark_connection_error(inner: &mut Inner, result: &Result<impl Sized>) {
    if matches!(result, Err(Error::Connection(_))) {
        inner.state.broken = true;
    }
}

/// [`Lease`] implementation over a [`LeaseBackend`] (see the module documentation).
pub struct ActorLease<B: LeaseBackend> {
    shared: Arc<Shared>,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    canceller: Arc<dyn Canceller>,
    task: Mutex<Option<JoinHandle<()>>>,
    tuning: LeaseTuning,
    report: tokio::sync::Mutex<Option<LeaseCloseReport>>,
    _backend: PhantomData<fn() -> B>,
}

impl<B: LeaseBackend> std::fmt::Debug for ActorLease<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorLease")
            .field("state", &self.shared.lock().state)
            .finish()
    }
}

impl<B: LeaseBackend> ActorLease<B> {
    /// Start the actor task owning `backend`, with the default timings. Must be called inside a
    /// tokio runtime.
    pub fn spawn(backend: B, opts: LeaseOptions) -> Result<ActorLease<B>> {
        Self::spawn_with(backend, opts, LeaseTuning::default())
    }

    /// Like [`ActorLease::spawn`] with explicit timings.
    pub fn spawn_with(
        backend: B,
        opts: LeaseOptions,
        tuning: LeaseTuning,
    ) -> Result<ActorLease<B>> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| Error::Other("no async runtime to run the lease on".into()))?;
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                state: LeaseState::initial(&opts),
                busy: false,
                closed: false,
            }),
        });
        let canceller = backend.canceller();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let task = handle.spawn(run_actor(backend, shared.clone(), cmd_rx, tuning));
        Ok(ActorLease {
            shared,
            cmd_tx,
            canceller,
            task: Mutex::new(Some(task)),
            tuning,
            report: tokio::sync::Mutex::new(None),
            _backend: PhantomData,
        })
    }

    /// Claim the command slot. `running` is the query id shown in the state.
    fn claim(&self, running: Option<Uuid>) -> Result<Slot<'_>> {
        let mut g = self.shared.lock();
        if g.closed {
            return Err(closed_error());
        }
        if g.state.broken {
            return Err(Error::Connection(
                "the lease connection is lost; close it and open a new one".into(),
            ));
        }
        if g.busy {
            return Err(Error::Busy("the lease is running another command".into()));
        }
        g.busy = true;
        g.state.running = running;
        Ok(Slot {
            shared: &self.shared,
            armed: true,
        })
    }

    fn send(&self, slot: Slot<'_>, cmd: Cmd) -> Result<()> {
        self.cmd_tx.send(cmd).map_err(|_| actor_gone())?;
        slot.sent();
        Ok(())
    }

    /// Run a command that answers with a plain result.
    async fn simple<T>(&self, build: impl FnOnce(oneshot::Sender<Result<T>>) -> Cmd) -> Result<T> {
        let slot = self.claim(None)?;
        let (tx, rx) = oneshot::channel();
        self.send(slot, build(tx))?;
        rx.await.map_err(|_| actor_gone())?
    }

    /// Forward the events of a streaming command to `sink` until the actor reports the end.
    async fn pump(
        &self,
        mut events: mpsc::UnboundedReceiver<LeaseEvent>,
        done: oneshot::Receiver<Result<ExecSummary>>,
        stop: Arc<AtomicBool>,
        sink: &mut dyn ResultSink,
        timeout: Option<Duration>,
        query_id: Uuid,
    ) -> Result<ExecSummary> {
        let mut done = Box::pin(done);
        let hard = tokio::time::sleep(Duration::from_secs(86_400 * 365));
        tokio::pin!(hard);
        let mut hard_armed = false;
        let timer = tokio::time::sleep(timeout.unwrap_or(Duration::from_secs(86_400 * 365)));
        tokio::pin!(timer);
        let mut timer_armed = timeout.is_some();
        let mut events_open = true;
        let mut timed_out = false;
        let mut sink_error: Option<Error> = None;
        let mut deliver = |event: LeaseEvent, sink_error: &mut Option<Error>| -> bool {
            if sink_error.is_some() {
                return false;
            }
            if let Err(e) = event.deliver(sink) {
                *sink_error = Some(e);
                return true;
            }
            false
        };
        let outcome = loop {
            tokio::select! {
                biased;
                event = events.recv(), if events_open => {
                    match event {
                        Some(event) => {
                            if deliver(event, &mut sink_error) {
                                // The sink wants to stop: the backend's next event is
                                // refused and it stops by itself. No protocol cancel here: in a
                                // transaction it would abort a healthy block.
                                stop.store(true, Ordering::Release);
                            }
                        }
                        // The actor dropped its sink; only the final result is left.
                        None => events_open = false,
                    }
                }
                result = &mut done => {
                    break result.unwrap_or_else(|_| Err(actor_gone()));
                }
                _ = &mut timer, if timer_armed => {
                    timer_armed = false;
                    timed_out = true;
                    hard.as_mut().reset(tokio::time::Instant::now() + self.tuning.timeout_grace);
                    hard_armed = true;
                    let _ = self.canceller.cancel_for(query_id).await;
                }
                _ = &mut hard, if hard_armed => {
                    // The cancel did not stop the statement: the connection cannot be trusted
                    // any more. Drop it; the lease is broken.
                    self.abandon();
                    break Err(Error::Timeout);
                }
            }
        };
        // Events sent just before the end are still in the channel.
        while let Ok(event) = events.try_recv() {
            deliver(event, &mut sink_error);
        }
        if let Some(e) = sink_error {
            return Err(e);
        }
        match outcome {
            Err(Error::Timeout) => Err(Error::Timeout),
            Err(Error::Connection(m)) => Err(Error::Connection(m)),
            Err(_) if timed_out => Err(Error::Timeout),
            other => other,
        }
    }
}

impl<B: LeaseBackend> ActorLease<B> {
    /// Stop the actor task (dropping the connection) and mark the lease broken.
    fn abandon(&self) {
        if let Some(task) = self.task.lock().unwrap_or_else(|e| e.into_inner()).take() {
            task.abort();
        }
        let mut g = self.shared.lock();
        g.state.broken = true;
        g.state.running = None;
        g.busy = false;
    }
}

impl<B: LeaseBackend> Drop for ActorLease<B> {
    fn drop(&mut self) {
        // Dropping without `close` closes the connection (the server rolls back).
        if let Some(task) = self
            .task
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            task.abort();
        }
    }
}

#[async_trait]
impl<B: LeaseBackend> Lease for ActorLease<B> {
    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        let query_id = req.query_id;
        let slot = self.claim(Some(query_id))?;
        let timeout = req.timeout_ms.map(Duration::from_millis);
        let keep_open = req.max_rows.is_some();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = oneshot::channel();
        let stop = Arc::new(AtomicBool::new(false));
        self.send(
            slot,
            Cmd::Execute {
                req,
                keep_open,
                events: events_tx,
                stop: stop.clone(),
                done: done_tx,
            },
        )?;
        self.pump(events_rx, done_rx, stop, sink, timeout, query_id)
            .await
    }

    async fn fetch_more(
        &self,
        query_id: Uuid,
        max_rows: u64,
        batch_size: Option<u32>,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        if self.shared.lock().state.pending_result != Some(query_id) {
            return Err(Error::NotFound(format!(
                "pending result of query {query_id}"
            )));
        }
        let slot = self.claim(Some(query_id))?;
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = oneshot::channel();
        let stop = Arc::new(AtomicBool::new(false));
        self.send(
            slot,
            Cmd::Fetch {
                max_rows,
                batch: batch_size.map_or(DEFAULT_FETCH_BATCH, |b| b.max(1) as usize),
                events: events_tx,
                stop: stop.clone(),
                done: done_tx,
            },
        )?;
        self.pump(events_rx, done_rx, stop, sink, None, query_id)
            .await
    }

    async fn discard_result(&self) -> Result<()> {
        if self.shared.lock().state.pending_result.is_none() {
            return Ok(());
        }
        self.simple(Cmd::Discard).await
    }

    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        if self.shared.lock().state.running != Some(query_id) {
            return Ok(());
        }
        self.canceller.cancel_for(query_id).await
    }

    async fn begin(&self, opts: TxOptions) -> Result<()> {
        if self.shared.lock().state.transaction != TxStatus::Idle {
            return Err(Error::Busy("a transaction is already open".into()));
        }
        self.simple(|reply| Cmd::Tx {
            op: TxControl::Begin,
            opts,
            reply,
        })
        .await
    }

    async fn commit(&self) -> Result<()> {
        self.simple(|reply| Cmd::Tx {
            op: TxControl::Commit,
            opts: TxOptions::default(),
            reply,
        })
        .await
    }

    async fn rollback(&self) -> Result<()> {
        self.simple(|reply| Cmd::Tx {
            op: TxControl::Rollback,
            opts: TxOptions::default(),
            reply,
        })
        .await
    }

    async fn set_auto_commit(&self, on: bool) -> Result<()> {
        if on
            && matches!(
                self.shared.lock().state.transaction,
                TxStatus::Active | TxStatus::Failed
            )
        {
            return Err(Error::Busy(
                "commit or roll back the open transaction before switching to auto-commit".into(),
            ));
        }
        self.simple(|reply| Cmd::AutoCommit { on, reply }).await
    }

    async fn set_context(&self, ctx: LeaseContext) -> Result<()> {
        self.simple(|reply| Cmd::Context { ctx, reply }).await
    }

    fn state(&self) -> LeaseState {
        self.shared.lock().state.clone()
    }

    async fn stream_table(
        &self,
        req: TableStreamRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let text = self
            .simple(|reply| Cmd::TableSelect {
                req: req.clone(),
                reply,
            })
            .await?;
        let mut query = QueryRequest::new(text);
        query.query_id = req.query_id;
        query.batch_size = req.batch_size;
        self.execute(query, sink).await
    }

    async fn bulk_insert(&self, req: BulkInsertRequest) -> Result<BulkInsertResult> {
        self.simple(|reply| Cmd::Bulk { req, reply }).await
    }

    async fn explain(&self, req: ExplainRequest) -> Result<RawPlan> {
        self.simple(|reply| Cmd::Explain { req, reply }).await
    }

    async fn ping(&self) -> Result<()> {
        {
            let g = self.shared.lock();
            if g.closed {
                return Err(closed_error());
            }
            // A running statement proves the connection is alive; an open result set must
            // not be disturbed (the pending-result timeout covers a dead connection).
            if g.busy || g.state.pending_result.is_some() {
                return Ok(());
            }
        }
        self.simple(Cmd::Ping).await
    }

    async fn close(&self) -> Result<LeaseCloseReport> {
        let mut cached = self.report.lock().await;
        if let Some(report) = *cached {
            return Ok(report);
        }
        let running = {
            let mut g = self.shared.lock();
            g.closed = true;
            g.state.running
        };
        if let Some(running_id) = running {
            let _ = tokio::time::timeout(
                self.tuning.cancel_wait,
                self.canceller.cancel_for(running_id),
            )
            .await;
        }
        let (tx, rx) = oneshot::channel();
        let budget = self.tuning.cancel_wait + self.tuning.rollback_wait + Duration::from_secs(1);
        let sent = self.cmd_tx.send(Cmd::Close(tx)).is_ok();
        let tx_open = self.shared.lock().state.transaction != TxStatus::Idle;
        let report = match sent {
            true => match tokio::time::timeout(budget, rx).await {
                Ok(Ok(report)) => report,
                // Timed out (or the actor died): stop it, which drops the connection.
                _ => LeaseCloseReport {
                    rolled_back: tx_open,
                    forced: true,
                },
            },
            false => LeaseCloseReport {
                rolled_back: tx_open,
                forced: true,
            },
        };
        if let Some(task) = self.task.lock().unwrap_or_else(|e| e.into_inner()).take() {
            if report.forced {
                task.abort();
            }
            // Otherwise the actor already left its loop.
        }
        {
            let mut g = self.shared.lock();
            g.busy = false;
            g.state.running = None;
            g.state.pending_result = None;
            g.state.transaction = TxStatus::Idle;
        }
        *cached = Some(report);
        Ok(report)
    }
}

/// Body of the actor task.
async fn run_actor<B: LeaseBackend>(
    mut backend: B,
    shared: Arc<Shared>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    tuning: LeaseTuning,
) {
    let mut pending_deadline: Option<Instant> = None;
    // When the last real command arrived. A keepalive `Ping` does not move it, so a forgotten
    // lease is still closed after `idle_ttl` however often the keepalive pings it.
    let mut last_activity = Instant::now();
    loop {
        let idle_deadline = tuning.idle_ttl.map(|d| last_activity + d);
        // `Instant` far in the future stands for "no deadline" in the guarded branches.
        let far = Instant::now() + Duration::from_secs(86_400 * 365);
        let cmd = tokio::select! {
            cmd = cmds.recv() => cmd,
            _ = tokio::time::sleep_until(pending_deadline.unwrap_or(far)), if pending_deadline.is_some() => {
                // Nobody fetched the pending result in time: release it.
                let _ = backend.discard_result().await;
                pending_deadline = None;
                shared.lock().state.pending_result = None;
                if let Ok(tx) = backend.probe_tx().await {
                    shared.lock().state.transaction = tx;
                }
                continue;
            }
            _ = tokio::time::sleep_until(idle_deadline.unwrap_or(far)), if idle_deadline.is_some() => {
                // Forgotten (or orphaned) lease: roll back and close the connection.
                let _ = tokio::time::timeout(tuning.rollback_wait, backend.close()).await;
                let mut g = shared.lock();
                g.closed = true;
                g.busy = false;
                g.state.running = None;
                g.state.pending_result = None;
                g.state.broken = true;
                break;
            }
        };
        let Some(cmd) = cmd else { break };
        if !matches!(cmd, Cmd::Ping(_)) {
            last_activity = Instant::now();
        }

        // Every command except a fetch drops the pending result first.
        // A fetch continues it and a keepalive ping must not destroy it.
        let keeps_pending = matches!(cmd, Cmd::Fetch { .. } | Cmd::Ping(_));
        if !keeps_pending && pending_deadline.take().is_some() {
            let _ = backend.discard_result().await;
            shared.lock().state.pending_result = None;
        }

        match cmd {
            Cmd::Execute {
                req,
                keep_open,
                events,
                stop,
                done,
            } => {
                let query_id = req.query_id;
                let text = req.text.clone();
                let mut sink = ChannelSink::new(events, stop);
                let result = backend.execute(req, &mut sink, keep_open).await;
                let pending = result.is_ok()
                    && backend
                        .has_pending()
                        .unwrap_or(keep_open && sink.ended_truncated());
                let context = if result.is_ok() || !matches!(result, Err(Error::Connection(_))) {
                    backend.refresh_context(&text).await.ok().flatten()
                } else {
                    None
                };
                finish_stream(&mut backend, &shared, &result, pending.then_some(query_id)).await;
                if let Some(ctx) = context {
                    apply_context(&shared, &ctx);
                }
                pending_deadline = pending.then(|| Instant::now() + pending_ttl(&backend, &tuning));
                let _ = done.send(result);
            }
            Cmd::Fetch {
                max_rows,
                batch,
                events,
                stop,
                done,
            } => {
                let query_id = shared.lock().state.pending_result;
                let mut sink = ChannelSink::new(events, stop);
                let result = if pending_deadline.is_none() {
                    Err(Error::NotFound("pending result".into()))
                } else {
                    backend.fetch_more(max_rows, batch, &mut sink).await
                };
                let pending = result.is_ok()
                    && backend
                        .has_pending()
                        .unwrap_or_else(|| sink.ended_truncated());
                finish_stream(
                    &mut backend,
                    &shared,
                    &result,
                    pending.then_some(query_id).flatten(),
                )
                .await;
                pending_deadline = pending.then(|| Instant::now() + pending_ttl(&backend, &tuning));
                let _ = done.send(result);
            }
            Cmd::Discard(reply) => {
                let result = backend.discard_result().await;
                finish_simple(&mut backend, &shared, &result).await;
                let _ = reply.send(result);
            }
            Cmd::Tx { op, opts, reply } => {
                let result = backend.tx(op, opts).await;
                finish_simple(&mut backend, &shared, &result).await;
                let _ = reply.send(result);
            }
            Cmd::AutoCommit { on, reply } => {
                let result = backend.set_auto_commit(on).await;
                if result.is_ok() {
                    shared.lock().state.auto_commit = on;
                }
                finish_simple(&mut backend, &shared, &result).await;
                let _ = reply.send(result);
            }
            Cmd::Context { ctx, reply } => {
                let result = backend.set_context(ctx.clone()).await;
                if result.is_ok() {
                    apply_context(&shared, &ctx);
                }
                finish_simple(&mut backend, &shared, &result).await;
                let _ = reply.send(result);
            }
            Cmd::TableSelect { req, reply } => {
                let result = backend.table_select(&req).await;
                finish_simple(&mut backend, &shared, &result).await;
                let _ = reply.send(result);
            }
            Cmd::Bulk { req, reply } => {
                let result = backend.bulk_insert(req).await;
                finish_simple(&mut backend, &shared, &result).await;
                let _ = reply.send(result);
            }
            Cmd::Explain { req, reply } => {
                let result = backend.explain(req).await;
                finish_simple(&mut backend, &shared, &result).await;
                let _ = reply.send(result);
            }
            Cmd::Ping(reply) => {
                let result = backend.ping().await;
                finish_simple(&mut backend, &shared, &result).await;
                let _ = reply.send(result);
            }
            Cmd::Close(reply) => {
                let tx_open = shared.lock().state.transaction != TxStatus::Idle;
                let report = match tokio::time::timeout(tuning.rollback_wait, backend.close()).await
                {
                    Ok(Ok(rolled_back)) => LeaseCloseReport {
                        rolled_back,
                        forced: false,
                    },
                    _ => LeaseCloseReport {
                        rolled_back: tx_open,
                        forced: true,
                    },
                };
                {
                    let mut g = shared.lock();
                    g.closed = true;
                    g.busy = false;
                    g.state.running = None;
                    g.state.pending_result = None;
                    g.state.transaction = TxStatus::Idle;
                }
                let _ = reply.send(report);
                break;
            }
        }
    }
}

fn pending_ttl<B: LeaseBackend>(backend: &B, tuning: &LeaseTuning) -> Duration {
    backend
        .pending_ttl()
        .map_or(tuning.pending_ttl, |t| t.min(tuning.pending_ttl))
}

fn apply_context(shared: &Shared, ctx: &LeaseContext) {
    let mut g = shared.lock();
    if let Some(db) = &ctx.database {
        g.state.database = Some(db.clone());
    }
    if let Some(schema) = &ctx.schema {
        g.state.schema = Some(schema.clone());
    }
}

/// Update the shared state after a streaming command and release the command slot.
async fn finish_stream<B: LeaseBackend>(
    backend: &mut B,
    shared: &Shared,
    result: &Result<ExecSummary>,
    pending: Option<Uuid>,
) {
    let broken = matches!(result, Err(Error::Connection(_)));
    let tx = if broken {
        None
    } else {
        Some(backend.probe_tx().await.unwrap_or(TxStatus::Unknown))
    };
    let mut g = shared.lock();
    mark_connection_error(&mut g, result);
    if let Some(tx) = tx {
        g.state.transaction = tx;
    }
    g.state.pending_result = pending;
    g.state.running = None;
    g.busy = false;
}

/// Same for the commands without a stream.
async fn finish_simple<B: LeaseBackend, T>(backend: &mut B, shared: &Shared, result: &Result<T>) {
    let broken = matches!(result, Err(Error::Connection(_)));
    let tx = if broken {
        None
    } else {
        Some(backend.probe_tx().await.unwrap_or(TxStatus::Unknown))
    };
    let mut g = shared.lock();
    mark_connection_error(&mut g, result);
    if let Some(tx) = tx {
        g.state.transaction = tx;
    }
    g.state.pending_result = None;
    g.state.running = None;
    g.busy = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CollectingSink, EndOutcome, StatementOutcome};
    use std::sync::atomic::AtomicUsize;

    /// Scripted in-memory backend: a table of `total` rows, a transaction flag and gates to
    /// make statements slow.
    struct Fake {
        total: u64,
        sent: u64,
        pending: bool,
        tx: TxStatus,
        auto_commit: bool,
        cancelled: Arc<AtomicBool>,
        discards: Arc<AtomicUsize>,
        closes: Arc<AtomicUsize>,
        /// Statement text "slow" blocks until cancelled.
        broken_on_text: Option<&'static str>,
    }

    struct FakeCancel(Arc<AtomicBool>);

    #[async_trait]
    impl Canceller for FakeCancel {
        async fn cancel(&self) -> Result<()> {
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    impl Fake {
        fn new(total: u64) -> (Fake, Arc<AtomicBool>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let cancelled = Arc::new(AtomicBool::new(false));
            let discards = Arc::new(AtomicUsize::new(0));
            let closes = Arc::new(AtomicUsize::new(0));
            (
                Fake {
                    total,
                    sent: 0,
                    pending: false,
                    tx: TxStatus::Idle,
                    auto_commit: true,
                    cancelled: cancelled.clone(),
                    discards: discards.clone(),
                    closes: closes.clone(),
                    broken_on_text: None,
                },
                cancelled,
                discards,
                closes,
            )
        }

        fn rows(&mut self, max: Option<u64>, sink: &mut dyn ResultSink) -> Result<()> {
            let want = max.unwrap_or(u64::MAX).min(self.total - self.sent);
            let rows: Vec<Vec<Value>> = (self.sent..self.sent + want)
                .map(|i| vec![Value::Int(i as i64)])
                .collect();
            self.sent += want;
            sink.on_rows(rows)?;
            let truncated = self.sent < self.total;
            self.pending = truncated;
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Rows {
                    row_count: want,
                    truncated,
                },
                elapsed_ms: 0,
            })
        }
    }

    #[async_trait]
    impl LeaseBackend for Fake {
        async fn execute(
            &mut self,
            req: QueryRequest,
            sink: &mut dyn ResultSink,
            keep_open: bool,
        ) -> Result<ExecSummary> {
            if Some(req.text.as_str()) == self.broken_on_text {
                return Err(Error::Connection("gone".into()));
            }
            if req.text == "slow" {
                while !self.cancelled.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                self.cancelled.store(false, Ordering::SeqCst);
                return Err(Error::Cancelled);
            }
            if req.text == "stuck" {
                std::future::pending::<()>().await;
            }
            if req.text == "begin-ish" || !self.auto_commit {
                self.tx = TxStatus::Active;
            }
            self.sent = 0;
            sink.on_columns(vec![])?;
            self.rows(req.max_rows, sink)?;
            if !keep_open {
                self.pending = false;
            }
            Ok(ExecSummary {
                statements: 1,
                elapsed_ms: 0,
            })
        }
        async fn fetch_more(
            &mut self,
            max_rows: u64,
            _batch: usize,
            sink: &mut dyn ResultSink,
        ) -> Result<ExecSummary> {
            self.rows(Some(max_rows), sink)?;
            Ok(ExecSummary::default())
        }
        async fn discard_result(&mut self) -> Result<()> {
            if self.pending {
                self.discards.fetch_add(1, Ordering::SeqCst);
            }
            self.pending = false;
            Ok(())
        }
        async fn tx(&mut self, op: TxControl, _: TxOptions) -> Result<()> {
            self.tx = match op {
                TxControl::Begin => TxStatus::Active,
                _ => TxStatus::Idle,
            };
            Ok(())
        }
        async fn set_auto_commit(&mut self, on: bool) -> Result<()> {
            self.auto_commit = on;
            Ok(())
        }
        async fn set_context(&mut self, _: LeaseContext) -> Result<()> {
            Ok(())
        }
        async fn probe_tx(&mut self) -> Result<TxStatus> {
            Ok(self.tx)
        }
        async fn refresh_context(&mut self, text: &str) -> Result<Option<LeaseContext>> {
            Ok(text.strip_prefix("use ").map(|db| LeaseContext {
                database: Some(db.into()),
                schema: None,
            }))
        }
        async fn ping(&mut self) -> Result<()> {
            Ok(())
        }
        async fn close(&mut self) -> Result<bool> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(self.tx != TxStatus::Idle)
        }
        fn canceller(&self) -> Arc<dyn Canceller> {
            Arc::new(FakeCancel(self.cancelled.clone()))
        }
    }

    fn lease(total: u64) -> (Arc<ActorLease<Fake>>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (fake, _, discards, closes) = Fake::new(total);
        let lease = ActorLease::spawn(fake, LeaseOptions::default()).unwrap();
        (Arc::new(lease), discards, closes)
    }

    fn request(text: &str, max_rows: Option<u64>) -> QueryRequest {
        let mut r = QueryRequest::new(text);
        r.max_rows = max_rows;
        r
    }

    fn row_count(sink: CollectingSink) -> (u64, bool) {
        match sink.into_results().remove(0).outcome {
            StatementOutcome::Rows(rs) => (rs.rows.len() as u64, rs.truncated),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_keepalive_ping_keeps_the_pending_result() {
        let (lease, discards, _) = lease(10);
        let req = request("select", Some(3));
        let id = req.query_id;
        lease
            .execute(req, &mut CollectingSink::new())
            .await
            .unwrap();
        assert_eq!(lease.state().pending_result, Some(id));
        lease.ping().await.unwrap();
        assert_eq!(lease.state().pending_result, Some(id));
        assert_eq!(discards.load(Ordering::SeqCst), 0);
        // The result is still fetchable after the ping.
        let mut sink = CollectingSink::new();
        lease.fetch_more(id, 3, None, &mut sink).await.unwrap();
        assert_eq!(row_count(sink), (3, true));
    }

    #[tokio::test]
    async fn execute_then_fetch_more_three_times() {
        let (lease, _, _) = lease(10);
        let req = request("select", Some(3));
        let id = req.query_id;
        let mut sink = CollectingSink::new();
        lease.execute(req, &mut sink).await.unwrap();
        assert_eq!(row_count(sink), (3, true));
        assert_eq!(lease.state().pending_result, Some(id));

        let mut got = Vec::new();
        for _ in 0..3 {
            let mut sink = CollectingSink::new();
            lease.fetch_more(id, 3, None, &mut sink).await.unwrap();
            got.push(row_count(sink));
        }
        assert_eq!(got, [(3, true), (3, true), (1, false)]);
        assert_eq!(lease.state().pending_result, None);
        // Nothing pending any more.
        let mut sink = CollectingSink::new();
        let err = lease.fetch_more(id, 3, None, &mut sink).await.unwrap_err();
        assert_eq!(err.kind(), "notFound");
    }

    #[tokio::test]
    async fn fetch_more_with_a_wrong_id_is_not_found() {
        let (lease, _, _) = lease(10);
        let req = request("select", Some(3));
        lease
            .execute(req, &mut CollectingSink::new())
            .await
            .unwrap();
        let err = lease
            .fetch_more(Uuid::new_v4(), 3, None, &mut CollectingSink::new())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "notFound");
        // The right result is still there.
        assert!(lease.state().pending_result.is_some());
    }

    #[tokio::test]
    async fn next_execute_and_discard_drop_the_pending_result() {
        let (lease, discards, _) = lease(10);
        lease
            .execute(request("select", Some(3)), &mut CollectingSink::new())
            .await
            .unwrap();
        lease.discard_result().await.unwrap();
        assert_eq!(lease.state().pending_result, None);
        assert_eq!(discards.load(Ordering::SeqCst), 1);
        // Discarding again is a no-op.
        lease.discard_result().await.unwrap();

        lease
            .execute(request("select", Some(3)), &mut CollectingSink::new())
            .await
            .unwrap();
        lease
            .execute(request("select", None), &mut CollectingSink::new())
            .await
            .unwrap();
        assert_eq!(discards.load(Ordering::SeqCst), 2);
        assert_eq!(lease.state().pending_result, None);
    }

    #[tokio::test]
    async fn a_second_call_while_running_is_busy_and_cancel_matches_the_query() {
        let (lease, _, _) = lease(1);
        let req = request("slow", None);
        let id = req.query_id;
        let runner = {
            let lease = lease.clone();
            tokio::spawn(async move { lease.execute(req, &mut CollectingSink::new()).await })
        };
        while lease.state().running.is_none() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(lease.state().running, Some(id));
        let err = lease
            .execute(request("select", None), &mut CollectingSink::new())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "busy");
        assert_eq!(lease.commit().await.unwrap_err().kind(), "busy");
        // A late / foreign id never cancels the running statement.
        lease.cancel(Uuid::new_v4()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(lease.state().running, Some(id));
        lease.cancel(id).await.unwrap();
        let result = runner.await.unwrap();
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(lease.state().running, None);
        // Cancelling a finished statement is fine.
        lease.cancel(id).await.unwrap();
        // The lease is usable again.
        lease
            .execute(request("select", None), &mut CollectingSink::new())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn timeout_cancels_and_reports_timeout() {
        let (lease, _, _) = lease(1);
        let mut req = request("slow", None);
        req.timeout_ms = Some(40);
        let started = std::time::Instant::now();
        let err = lease
            .execute(req, &mut CollectingSink::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Timeout), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(lease.state().running, None);
    }

    #[tokio::test]
    async fn a_failing_sink_stops_the_statement_and_its_error_wins() {
        struct Angry;
        impl ResultSink for Angry {
            fn on_columns(&mut self, _: Vec<ColumnMeta>) -> Result<()> {
                Err(Error::Other("sink closed".into()))
            }
            fn on_rows(&mut self, _: Vec<Vec<Value>>) -> Result<()> {
                Ok(())
            }
            fn on_message(&mut self, _: String) -> Result<()> {
                Ok(())
            }
            fn on_statement_end(&mut self, _: StatementEnd) -> Result<()> {
                Ok(())
            }
        }
        let (lease, _, _) = lease(5);
        let err = lease
            .execute(request("select", None), &mut Angry)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "sink closed");
        // The lease keeps working.
        lease
            .execute(request("select", None), &mut CollectingSink::new())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn transaction_state_is_cached_and_auto_commit_switch_is_guarded() {
        let (lease, _, _) = lease(1);
        assert_eq!(lease.state().transaction, TxStatus::Idle);
        lease.begin(TxOptions::default()).await.unwrap();
        assert_eq!(lease.state().transaction, TxStatus::Active);
        assert_eq!(
            lease.begin(TxOptions::default()).await.unwrap_err().kind(),
            "busy"
        );
        assert_eq!(
            lease.set_auto_commit(true).await.unwrap_err().kind(),
            "busy"
        );
        lease.set_auto_commit(false).await.unwrap();
        assert!(!lease.state().auto_commit);
        lease.rollback().await.unwrap();
        assert_eq!(lease.state().transaction, TxStatus::Idle);
        lease.set_auto_commit(true).await.unwrap();
        assert!(lease.state().auto_commit);
        // Manual mode: a statement opens the transaction.
        lease.set_auto_commit(false).await.unwrap();
        lease
            .execute(request("select", None), &mut CollectingSink::new())
            .await
            .unwrap();
        assert_eq!(lease.state().transaction, TxStatus::Active);
    }

    #[tokio::test]
    async fn context_follows_set_context_and_executed_scripts() {
        let (lease, _, _) = lease(1);
        lease
            .set_context(LeaseContext {
                database: Some("a".into()),
                schema: Some("s".into()),
            })
            .await
            .unwrap();
        let s = lease.state();
        assert_eq!(
            (s.database.as_deref(), s.schema.as_deref()),
            (Some("a"), Some("s"))
        );
        lease
            .execute(request("use b", None), &mut CollectingSink::new())
            .await
            .unwrap();
        assert_eq!(lease.state().database.as_deref(), Some("b"));
        assert_eq!(lease.state().schema.as_deref(), Some("s"));
    }

    #[tokio::test]
    async fn connection_errors_break_the_lease() {
        let (mut fake, _, _, _) = Fake::new(1);
        fake.broken_on_text = Some("boom");
        let lease = ActorLease::spawn(fake, LeaseOptions::default()).unwrap();
        let err = lease
            .execute(request("boom", None), &mut CollectingSink::new())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "connection");
        assert!(lease.state().broken);
        let again = lease
            .execute(request("select", None), &mut CollectingSink::new())
            .await
            .unwrap_err();
        assert_eq!(again.kind(), "connection");
        // Only close is valid, and it works.
        let report = lease.close().await.unwrap();
        assert!(!report.forced);
    }

    #[tokio::test]
    async fn close_reports_rollback_is_idempotent_and_refuses_further_calls() {
        let (lease, _, closes) = lease(1);
        lease.begin(TxOptions::default()).await.unwrap();
        let report = lease.close().await.unwrap();
        assert_eq!(
            report,
            LeaseCloseReport {
                rolled_back: true,
                forced: false
            }
        );
        assert_eq!(lease.close().await.unwrap(), report);
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert_eq!(lease.state().transaction, TxStatus::Idle);
        let err = lease
            .execute(request("select", None), &mut CollectingSink::new())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "connection");
        assert!(lease.ping().await.is_err());
    }

    #[tokio::test]
    async fn close_cancels_the_running_statement() {
        let (lease, _, closes) = lease(1);
        let runner = {
            let lease = lease.clone();
            tokio::spawn(async move {
                lease
                    .execute(request("slow", None), &mut CollectingSink::new())
                    .await
            })
        };
        while lease.state().running.is_none() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let report = lease.close().await.unwrap();
        assert!(!report.forced);
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert!(runner.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn close_gives_up_on_a_stuck_statement_and_reports_forced() {
        let (fake, _, _, closes) = Fake::new(1);
        let tuning = LeaseTuning {
            pending_ttl: Duration::from_secs(60),
            cancel_wait: Duration::from_millis(100),
            rollback_wait: Duration::from_millis(100),
            ..LeaseTuning::default()
        };
        let lease =
            Arc::new(ActorLease::spawn_with(fake, LeaseOptions::default(), tuning).unwrap());
        {
            let lease = lease.clone();
            tokio::spawn(async move {
                let _ = lease
                    .execute(request("stuck", None), &mut CollectingSink::new())
                    .await;
            });
        }
        while lease.state().running.is_none() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let started = std::time::Instant::now();
        let report = lease.close().await.unwrap();
        assert!(report.forced);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(closes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_forgotten_pending_result_is_discarded_after_the_ttl() {
        let (fake, _, discards, _) = Fake::new(10);
        let tuning = LeaseTuning {
            pending_ttl: Duration::from_millis(150),
            ..LeaseTuning::default()
        };
        let lease = ActorLease::spawn_with(fake, LeaseOptions::default(), tuning).unwrap();
        lease
            .execute(request("select", Some(3)), &mut CollectingSink::new())
            .await
            .unwrap();
        assert!(lease.state().pending_result.is_some());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(lease.state().pending_result.is_some());
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(lease.state().pending_result, None);
        assert_eq!(discards.load(Ordering::SeqCst), 1);
        // Fetching the expired result is refused.
        let err = lease
            .fetch_more(Uuid::new_v4(), 3, None, &mut CollectingSink::new())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "notFound");
    }

    #[tokio::test]
    async fn dropping_the_handle_stops_the_actor() {
        let (fake, _, _, _) = Fake::new(1);
        let lease = ActorLease::spawn(fake, LeaseOptions::default()).unwrap();
        let tx = lease.cmd_tx.clone();
        drop(lease);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(tx.is_closed());
    }

    #[tokio::test]
    async fn ping_works_while_idle_and_while_running() {
        let (lease, _, _) = lease(1);
        lease.ping().await.unwrap();
        let runner = {
            let lease = lease.clone();
            tokio::spawn(async move {
                lease
                    .execute(request("slow", None), &mut CollectingSink::new())
                    .await
            })
        };
        while lease.state().running.is_none() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        lease.ping().await.unwrap();
        let id = lease.state().running.unwrap();
        lease.cancel(id).await.unwrap();
        let _ = runner.await.unwrap();
    }

    #[tokio::test]
    async fn a_timeout_whose_cancel_does_not_work_breaks_the_lease() {
        let (fake, _, _, _) = Fake::new(1);
        let tuning = LeaseTuning {
            timeout_grace: Duration::from_millis(150),
            ..LeaseTuning::default()
        };
        let lease = ActorLease::spawn_with(fake, LeaseOptions::default(), tuning).unwrap();
        let mut req = request("stuck", None);
        req.timeout_ms = Some(50);
        let started = std::time::Instant::now();
        let err = lease
            .execute(req, &mut CollectingSink::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Timeout), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(lease.state().broken);
        assert_eq!(lease.state().running, None);
        // Only close is valid; it does not hang.
        let report = lease.close().await.unwrap();
        assert!(report.forced);
    }

    #[tokio::test]
    async fn an_idle_lease_is_rolled_back_and_closed() {
        let (fake, _, _, closes) = Fake::new(1);
        let tuning = LeaseTuning {
            idle_ttl: Some(Duration::from_millis(120)),
            ..LeaseTuning::default()
        };
        let lease = ActorLease::spawn_with(fake, LeaseOptions::default(), tuning).unwrap();
        lease.begin(TxOptions::default()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert!(lease.state().broken);
        let err = lease
            .execute(request("select", None), &mut CollectingSink::new())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "connection");
    }

    #[tokio::test]
    async fn activity_keeps_a_lease_alive() {
        let (fake, _, _, closes) = Fake::new(1);
        let tuning = LeaseTuning {
            idle_ttl: Some(Duration::from_millis(250)),
            ..LeaseTuning::default()
        };
        let lease = ActorLease::spawn_with(fake, LeaseOptions::default(), tuning).unwrap();
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            lease
                .execute(request("select", None), &mut CollectingSink::new())
                .await
                .unwrap();
        }
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert!(!lease.state().broken);
    }

    #[tokio::test]
    async fn keepalive_pings_do_not_renew_the_idle_limit() {
        let (fake, _, _, closes) = Fake::new(1);
        let tuning = LeaseTuning {
            idle_ttl: Some(Duration::from_millis(250)),
            ..LeaseTuning::default()
        };
        let lease = ActorLease::spawn_with(fake, LeaseOptions::default(), tuning).unwrap();
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = lease.ping().await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(closes.load(Ordering::SeqCst), 1, "idle_ttl still applies");
        assert!(lease.state().broken);
    }
}
