//! Connections, the small pool, and the bridge between blocking OCI calls and async sinks.
//!
//! ODPI-C is synchronous. Every database call runs on tokio's blocking pool; results travel from
//! the worker to the async side through a bounded channel so the sink (which may be slow, or stop
//! the stream) is called from async code and applies back pressure.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dbstudio_core::{ColumnMeta, Error, Result, ResultSink, StatementEnd, StatementStart, Value};
use oracle::{Connection, Connector};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use uuid::Uuid;

use crate::error::map_error;
use crate::target::Target;

/// Largest number of physical connections of one session (leases are not counted).
pub const POOL_SIZE: usize = 4;
/// How long an interrupted call may take to stop before the caller gives up waiting for it.
///
/// `break_execution` is best effort: when the server cannot honour the break while it is inside
/// a call (out-of-band data lost on the way, a non-interruptible wait such as a sleep), the
/// break is only seen once that call returns. Cancel and timeout must still answer the user.
pub const INTERRUPT_GRACE: Duration = Duration::from_secs(5);
/// Pooled connections idle for longer than this are pinged before use.
const PING_AFTER: Duration = Duration::from_secs(30);

/// Flags shared between the async side and the worker of one running call.
#[derive(Debug, Default)]
pub struct Control {
    pub cancelled: AtomicBool,
    pub timed_out: AtomicBool,
    /// The call did not stop after being interrupted and was given up on (its worker is still
    /// blocked inside OCI): the connection must not be used again.
    pub abandoned: AtomicBool,
}

impl Control {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn is_abandoned(&self) -> bool {
        self.abandoned.load(Ordering::SeqCst)
    }
}

/// Sink that discards everything (calls that only need the cancel / timeout machinery).
#[derive(Debug, Default)]
pub struct NullSink;

impl ResultSink for NullSink {
    fn on_columns(&mut self, _: Vec<ColumnMeta>) -> Result<()> {
        Ok(())
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

/// What a worker reports to the sink.
#[derive(Debug)]
pub enum Event {
    Start(StatementStart),
    Columns(Vec<ColumnMeta>),
    Rows(Vec<Vec<Value>>),
    Message(String),
    End(StatementEnd),
}

/// Sender used by the worker thread.
#[derive(Debug, Clone)]
pub struct Emitter {
    tx: mpsc::Sender<Event>,
}

impl Emitter {
    /// Send an event; fails with `Cancelled` when the consumer is gone.
    pub fn send(&self, event: Event) -> Result<()> {
        self.tx.blocking_send(event).map_err(|_| Error::Cancelled)
    }
}

#[derive(Debug)]
struct Idle {
    conn: Arc<Connection>,
    since: Instant,
}

/// A registered call: its connection and flags.
type Running = (Arc<Connection>, Arc<Control>);

/// Shared state of one session: how to connect, the idle connections and the running calls.
#[derive(Debug)]
pub struct Shared {
    pub target: Arc<Target>,
    pub read_only: bool,
    idle: Mutex<Vec<Idle>>,
    permits: Arc<Semaphore>,
    running: Mutex<HashMap<Uuid, Running>>,
    closed: AtomicBool,
}

/// A connection borrowed from the pool; goes back to it on drop unless marked broken.
#[derive(Debug)]
pub struct Pooled {
    conn: Option<Arc<Connection>>,
    shared: Arc<Shared>,
    _permit: OwnedSemaphorePermit,
    discard: bool,
}

impl Pooled {
    pub fn conn(&self) -> &Arc<Connection> {
        self.conn.as_ref().expect("connection present until drop")
    }

    /// Do not reuse this connection.
    pub fn discard(&mut self) {
        self.discard = true;
    }
}

impl Drop for Pooled {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else {
            return;
        };
        if self.discard || self.shared.closed.load(Ordering::SeqCst) {
            return;
        }
        let mut idle = self.shared.idle.lock().unwrap_or_else(|e| e.into_inner());
        idle.push(Idle {
            conn,
            since: Instant::now(),
        });
    }
}

/// Session setup statements run on every new connection.
const INIT_SQL: [&str; 2] = [
    "ALTER SESSION SET TIME_ZONE = 'UTC' NLS_NUMERIC_CHARACTERS = '.,' \
     NLS_DATE_FORMAT = 'YYYY-MM-DD HH24:MI:SS' NLS_TIMESTAMP_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF' \
     NLS_TIMESTAMP_TZ_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM'",
    "BEGIN DBMS_OUTPUT.ENABLE(NULL); END;",
];

/// Open and initialise one physical connection (blocking).
pub fn open_connection(target: &Target) -> Result<Connection> {
    let mut connector = Connector::new(
        target.user.clone(),
        target.password().to_string(),
        target.connect_string.clone(),
    );
    if let Some(p) = target.privilege {
        connector.privilege(p);
    }
    connector.stmt_cache_size(50);
    connector.driver_name("DBStudio");
    // Whatever else goes wrong while connecting is a connection problem, not a query error.
    let conn = connector.connect().map_err(|e| match map_error(e) {
        Error::Query { message, .. } => Error::Connection(message),
        other => other,
    })?;
    for sql in INIT_SQL {
        // Session tweaks are best effort (a restricted login may refuse them).
        let _ = conn.execute(sql, &[]);
    }
    let _ = conn.set_module("DBStudio");
    Ok(conn)
}

impl Shared {
    pub fn new(target: Target, read_only: bool) -> Arc<Shared> {
        Arc::new(Shared {
            target: Arc::new(target),
            read_only,
            idle: Mutex::new(Vec::new()),
            permits: Arc::new(Semaphore::new(POOL_SIZE)),
            running: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        })
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Open a dedicated connection that is not part of the pool (for leases).
    pub async fn open_dedicated(self: &Arc<Self>) -> Result<Arc<Connection>> {
        if self.is_closed() {
            return Err(Error::Connection("the session is closed".into()));
        }
        let target = self.target.clone();
        connect_with_timeout(target).await.map(Arc::new)
    }

    /// Borrow a connection (opening one when none is idle).
    pub async fn acquire(self: &Arc<Self>) -> Result<Pooled> {
        if self.is_closed() {
            return Err(Error::Connection("the session is closed".into()));
        }
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Connection("the session is closed".into()))?;
        loop {
            let candidate = {
                let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
                idle.pop()
            };
            match candidate {
                Some(idle) => {
                    if idle.since.elapsed() > PING_AFTER {
                        let c = idle.conn.clone();
                        let alive = tokio::task::spawn_blocking(move || c.ping().is_ok())
                            .await
                            .unwrap_or(false);
                        if !alive {
                            continue;
                        }
                    }
                    return Ok(Pooled {
                        conn: Some(idle.conn),
                        shared: self.clone(),
                        _permit: permit,
                        discard: false,
                    });
                }
                None => {
                    let conn = connect_with_timeout(self.target.clone()).await?;
                    return Ok(Pooled {
                        conn: Some(Arc::new(conn)),
                        shared: self.clone(),
                        _permit: permit,
                        discard: false,
                    });
                }
            }
        }
    }

    /// Run `f` on a pooled connection (blocking pool). A lost connection is dropped.
    pub async fn with_conn<T, F>(self: &Arc<Self>, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let mut pooled = self.acquire().await?;
        let conn = pooled.conn().clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let r = f(&conn);
            if r.is_err() {
                // Leave no open transaction behind after a failure.
                let _ = conn.rollback();
            }
            r
        })
        .await
        .map_err(|e| Error::Other(format!("worker failed: {e}")))?;
        if let Err(Error::Connection(_)) = &outcome {
            pooled.discard();
        }
        outcome
    }

    /// Register a running call so that [`Shared::cancel`] can reach it.
    pub fn register(&self, id: Uuid, conn: Arc<Connection>, ctl: Arc<Control>) {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, (conn, ctl));
    }

    pub fn unregister(&self, id: Uuid) {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }

    /// Interrupt the call `id` (`Ok` when it is unknown or already finished).
    pub async fn cancel(&self, id: Uuid) -> Result<()> {
        let entry = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned();
        if let Some((conn, ctl)) = entry {
            ctl.cancelled.store(true, Ordering::SeqCst);
            tokio::task::spawn_blocking(move || conn.break_execution())
                .await
                .map_err(|e| Error::Other(format!("worker failed: {e}")))?
                .map_err(map_error)?;
        }
        Ok(())
    }

    /// Close the session: interrupt running calls and drop the idle connections.
    pub async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.permits.close();
        let running: Vec<_> = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for (conn, ctl) in running {
            ctl.cancelled.store(true, Ordering::SeqCst);
            let _ = tokio::task::spawn_blocking(move || conn.break_execution()).await;
        }
        let idle: Vec<Idle> =
            std::mem::take(&mut *self.idle.lock().unwrap_or_else(|e| e.into_inner()));
        let _ = tokio::task::spawn_blocking(move || {
            for i in idle {
                let _ = i.conn.close();
            }
        })
        .await;
    }
}

async fn connect_with_timeout(target: Arc<Target>) -> Result<Connection> {
    let timeout = target.timeout;
    let task = tokio::task::spawn_blocking(move || open_connection(&target));
    match tokio::time::timeout(timeout + Duration::from_secs(2), task).await {
        Ok(joined) => joined.map_err(|e| Error::Other(format!("worker failed: {e}")))?,
        Err(_) => Err(Error::Connection(format!(
            "connecting timed out after {} ms",
            timeout.as_millis()
        ))),
    }
}

/// Run `work` on the blocking pool and forward its events to `sink`.
///
/// * a sink error stops the stream: the channel is closed (the worker fails on its next send) and
///   that error is returned once the worker has ended,
/// * `timeout` interrupts the running call with `break_execution` and yields `Error::Timeout`,
/// * `ctl.cancelled` (set by [`Shared::cancel`]) turns the worker's `ORA-01013` into `Cancelled`.
pub async fn stream<T, F>(
    conn: Arc<Connection>,
    ctl: Arc<Control>,
    timeout: Option<Duration>,
    sink: &mut dyn ResultSink,
    work: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection, &Emitter, &Control) -> Result<T> + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Event>(16);
    let emitter = Emitter { tx };
    let worker_conn = conn.clone();
    let worker_ctl = ctl.clone();
    let handle = tokio::task::spawn_blocking(move || work(&worker_conn, &emitter, &worker_ctl));
    let sleep = tokio::time::sleep(timeout.unwrap_or(Duration::from_secs(86_400 * 365)));
    tokio::pin!(sleep);
    let mut timer_armed = timeout.is_some();
    let mut sink_error: Option<Error> = None;
    let mut interrupted_at: Option<Instant> = None;
    let mut watch = tokio::time::interval(Duration::from_millis(250));
    watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut gave_up = false;
    loop {
        tokio::select! {
            _ = watch.tick() => {
                if ctl.cancelled.load(Ordering::SeqCst) || ctl.timed_out.load(Ordering::SeqCst) {
                    let since = *interrupted_at.get_or_insert_with(Instant::now);
                    if since.elapsed() >= INTERRUPT_GRACE {
                        // The worker is still blocked in OCI: leave it to finish on its own.
                        ctl.abandoned.store(true, Ordering::SeqCst);
                        rx.close();
                        gave_up = true;
                        break;
                    }
                }
            }
            event = rx.recv() => {
                let Some(event) = event else { break };
                if sink_error.is_some() {
                    continue;
                }
                let delivered = match event {
                    Event::Start(s) => sink.on_statement_start(s),
                    Event::Columns(c) => sink.on_columns(c),
                    Event::Rows(r) => sink.on_rows(r),
                    Event::Message(m) => sink.on_message(m),
                    Event::End(e) => sink.on_statement_end(e),
                };
                if let Err(e) = delivered {
                    sink_error = Some(e);
                    rx.close();
                }
            }
            () = &mut sleep, if timer_armed => {
                timer_armed = false;
                ctl.timed_out.store(true, Ordering::SeqCst);
                let c = conn.clone();
                let _ = tokio::task::spawn_blocking(move || c.break_execution()).await;
            }
        }
    }
    if gave_up {
        return Err(if ctl.timed_out.load(Ordering::SeqCst) {
            Error::Timeout
        } else {
            Error::Cancelled
        });
    }
    let result = handle
        .await
        .map_err(|e| Error::Other(format!("worker failed: {e}")))?;
    if let Some(e) = sink_error {
        return Err(e);
    }
    result.map_err(|e| {
        crate::error::classify_interrupt(
            e,
            ctl.cancelled.load(Ordering::SeqCst),
            ctl.timed_out.load(Ordering::SeqCst),
        )
    })
}
