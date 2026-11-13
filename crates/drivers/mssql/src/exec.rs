//! Running scripts: batches (`GO [n]`), result sets, `PRINT` messages and per-statement counts
//! streamed into a [`ResultSink`].
//!
//! Each statement the server reports becomes one statement of the sink: a result set (`Metadata`
//! ... rows ... `DONE` with count) is `Rows`, a `DONE` with a row count and no result set is
//! `Affected`, any other statement the server reports is `Done`. `INFO` tokens (`PRINT`,
//! `RAISERROR` below severity 11, ...) go to `on_message`.
//!
//! Cancelling uses the TDS attention packet (see the vendored `tiberius`): the stream keeps
//! reading until the server acknowledges, so the connection stays usable. Reaching `max_rows`
//! stops the batch the same way, unless a [`Gate`] is supplied (leases): then the executor
//! pauses and waits for `fetch_more` / discard.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dbstudio_core::{
    ColumnMeta, EndOutcome, Error, ExecSummary, Result, ResultSink, StatementEnd, StatementStart,
    TextRange, Value,
};
use futures_util::StreamExt;
use tiberius::{DoneKind, QueryItem, ServerMessage};
use tokio::sync::{Notify, mpsc};

use crate::batch::{Batch, line_start_offset, split_batches};
use crate::conn::Conn;
use crate::error::{self, error_line, map_query};
use crate::value::{self, column_meta};

/// How long the server gets to acknowledge a cancel before the connection is given up.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Receiver of the events of a run. Async so that a lease can forward them through a bounded
/// channel (back pressure); [`SinkOut`] adapts a plain [`ResultSink`].
#[async_trait]
pub trait Out: Send {
    async fn start(&mut self, s: StatementStart) -> Result<()>;
    async fn columns(&mut self, c: Vec<ColumnMeta>) -> Result<()>;
    async fn rows(&mut self, r: Vec<Vec<Value>>) -> Result<()>;
    async fn message(&mut self, m: String) -> Result<()>;
    async fn end(&mut self, e: StatementEnd) -> Result<()>;
}

/// [`Out`] over a [`ResultSink`].
pub struct SinkOut<'a>(pub &'a mut dyn ResultSink);

#[async_trait]
impl Out for SinkOut<'_> {
    async fn start(&mut self, s: StatementStart) -> Result<()> {
        self.0.on_statement_start(s)
    }
    async fn columns(&mut self, c: Vec<ColumnMeta>) -> Result<()> {
        self.0.on_columns(c)
    }
    async fn rows(&mut self, r: Vec<Vec<Value>>) -> Result<()> {
        self.0.on_rows(r)
    }
    async fn message(&mut self, m: String) -> Result<()> {
        self.0.on_message(m)
    }
    async fn end(&mut self, e: StatementEnd) -> Result<()> {
        self.0.on_statement_end(e)
    }
}

/// Cancellation signal shared by the caller of [`run_script`] and whoever cancels.
#[derive(Debug, Default)]
pub struct Control {
    cancelled: AtomicBool,
    notify: Notify,
}

impl Control {
    /// Request cancellation (the caller also fires the connection's attention).
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    /// Whether [`Control::cancel`] was called.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Parameters of one run.
#[derive(Debug, Clone)]
pub struct RunParams {
    /// Rows kept per result set.
    pub max_rows: Option<u64>,
    /// Rows per `on_rows` call.
    pub batch_size: usize,
    /// Client side time limit.
    pub timeout: Option<Duration>,
}

/// Message to a paused executor.
#[derive(Debug)]
pub enum GateMsg {
    /// Deliver up to this many more rows of the paused result set.
    Continue(u64),
    /// Drop the rest of the result set (cancels the batch).
    Discard,
}

/// Lets a lease pause a run at `max_rows` instead of cancelling it.
pub struct Gate {
    pub rx: mpsc::Receiver<GateMsg>,
    /// Called after the end event of the truncated segment was delivered.
    pub paused: Box<dyn FnMut() + Send>,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Gate")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reason {
    User,
    Timeout,
    /// `max_rows` reached or the sink asked to stop.
    Soft,
}

struct OpenSet {
    /// Rows of the current segment.
    count: u64,
    buf: Vec<Vec<Value>>,
    started: Instant,
    /// Rows allowed in the current segment.
    limit: u64,
}

struct Totals {
    statements: u32,
    last_end: Instant,
}

enum BatchEnd {
    Completed,
    /// The batch was cut short (cancel or `max_rows`).
    Stopped(Option<Reason>),
}

/// Run `text` on `conn`. Returns `Err(Cancelled)` / `Err(Timeout)` when stopped by cancel or the
/// time limit; the connection stays usable unless `conn.broken` is set.
pub async fn run_script(
    conn: &mut Conn,
    text: &str,
    p: &RunParams,
    ctl: &Control,
    mut gate: Option<&mut Gate>,
    out: &mut dyn Out,
) -> Result<ExecSummary> {
    let started = Instant::now();
    let batches = split_batches(text);
    let mut totals = Totals {
        statements: 0,
        last_end: started,
    };
    let deadline = p.timeout.map(|d| started + d);
    let mut reason: Option<Reason> = None;
    'outer: for batch in &batches {
        for _ in 0..batch.repeat {
            if ctl.is_cancelled() {
                return Err(Error::Cancelled);
            }
            match run_batch(
                conn,
                batch,
                p,
                ctl,
                deadline,
                gate.as_deref_mut(),
                out,
                &mut totals,
            )
            .await?
            {
                BatchEnd::Completed => {}
                BatchEnd::Stopped(r) => {
                    // `max_rows` stops this batch only; a cancel or timeout stops the script.
                    if r != Some(Reason::Soft) {
                        reason = r;
                        break 'outer;
                    }
                    break;
                }
            }
        }
    }
    match reason {
        Some(Reason::User) => Err(Error::Cancelled),
        Some(Reason::Timeout) => Err(Error::Timeout),
        _ => Ok(ExecSummary {
            statements: totals.statements,
            elapsed_ms: started.elapsed().as_millis() as u64,
        }),
    }
}

fn message_text(m: &ServerMessage) -> String {
    m.message.clone()
}

#[allow(clippy::too_many_arguments)]
async fn run_batch(
    conn: &mut Conn,
    batch: &Batch,
    p: &RunParams,
    ctl: &Control,
    deadline: Option<Instant>,
    mut gate: Option<&mut Gate>,
    out: &mut dyn Out,
    totals: &mut Totals,
) -> Result<BatchEnd> {
    let handle = conn.cancel.clone();
    let lob = conn.lob_limit;
    let range = TextRange {
        start: batch.start,
        end: batch.start + batch.len,
    };
    let mut stream = match conn.client.simple_query(batch.text.as_str()).await {
        Ok(s) => s,
        Err(e) => {
            conn.broken |= error::is_fatal(&e);
            let pos = error_line(&e).map(|l| batch.start + line_start_offset(&batch.text, l));
            return Err(map_query(e, pos));
        }
    };
    // A cancel that arrived before the request was on the wire was wiped by the send.
    if ctl.is_cancelled() {
        handle.cancel();
    }

    let timeout_sleep = async {
        match deadline {
            Some(d) => tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(timeout_sleep);
    let mut timeout_armed = true;
    let drain_sleep = tokio::time::sleep(Duration::from_secs(24 * 3600 * 365));
    tokio::pin!(drain_sleep);

    let mut reason: Option<Reason> = None;
    let mut sink_error: Option<Error> = None;
    let mut open: Option<OpenSet> = None;
    let mut carry: Option<tiberius::Row> = None;
    let mut drain_armed = false;

    // Begin stopping: remember why, fire the attention and bound the wait for its answer.
    macro_rules! stop {
        ($why:expr) => {{
            if reason.is_none() {
                reason = Some($why);
                handle.cancel();
                drain_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + DRAIN_TIMEOUT);
                drain_armed = true;
            }
        }};
    }
    macro_rules! emit {
        ($e:expr) => {
            if sink_error.is_none() {
                if let Err(err) = $e.await {
                    sink_error = Some(err);
                    stop!(Reason::Soft);
                }
            }
        };
    }

    loop {
        // Rows kept back at a pause are processed before reading further.
        let item = if let Some(row) = carry.take() {
            Some(Ok(QueryItem::Row(row)))
        } else {
            tokio::select! {
                biased;
                _ = &mut timeout_sleep, if timeout_armed => {
                    timeout_armed = false;
                    stop!(Reason::Timeout);
                    continue;
                }
                _ = ctl.notify.notified(), if reason.is_none() => {
                    stop!(Reason::User);
                    continue;
                }
                _ = &mut drain_sleep, if drain_armed => {
                    conn.broken = true;
                    drop(stream);
                    return Err(Error::Connection(
                        "the server did not acknowledge the cancel request in time".into(),
                    ));
                }
                next = stream.next() => next,
            }
        };
        let Some(item) = item else { break };
        let item = match item {
            Ok(i) => i,
            Err(e) => {
                let line_pos = error_line(&e)
                    .map(|l| batch.start + line_start_offset(&batch.text, l));
                // Deliver what was read before the error, then report it.
                if let Some(set) = open.take() {
                    if reason.is_none() && sink_error.is_none() {
                        let _ = finish_set(out, set, false, totals).await;
                    }
                }
                drop(stream);
                if error::is_fatal(&e) {
                    conn.broken = true;
                }
                if let Some(se) = sink_error {
                    return Err(se);
                }
                return Err(match reason {
                    Some(Reason::User) => Error::Cancelled,
                    Some(Reason::Timeout) => Error::Timeout,
                    _ => map_query(e, line_pos),
                });
            }
        };
        match item {
            QueryItem::Metadata(m) => {
                if reason.is_some() {
                    continue;
                }
                if let Some(set) = open.take() {
                    emit!(finish_set(out, set, false, totals));
                }
                let cols = m.columns().iter().map(column_meta).collect();
                emit!(out.start(StatementStart {
                    index: totals.statements,
                    range,
                }));
                emit!(out.columns(cols));
                open = Some(OpenSet {
                    count: 0,
                    buf: Vec::new(),
                    started: totals.last_end,
                    limit: p.max_rows.unwrap_or(u64::MAX),
                });
            }
            QueryItem::Row(row) => {
                if reason.is_some() {
                    continue;
                }
                let Some(set) = open.as_mut() else { continue };
                if set.count >= set.limit {
                    // One row beyond the limit: more data exists.
                    match gate.as_deref_mut() {
                        Some(g) => {
                            carry = Some(row);
                            let set = open.take().expect("set is open");
                            emit!(finish_set(out, set, true, totals));
                            (g.paused)();
                            match g.rx.recv().await {
                                Some(GateMsg::Continue(n)) => {
                                    emit!(out.start(StatementStart {
                                        index: totals.statements,
                                        range,
                                    }));
                                    open = Some(OpenSet {
                                        count: 0,
                                        buf: Vec::new(),
                                        started: Instant::now(),
                                        limit: n.max(1),
                                    });
                                }
                                Some(GateMsg::Discard) | None => {
                                    carry = None;
                                    stop!(Reason::Soft);
                                }
                            }
                        }
                        None => {
                            let set = open.take().expect("set is open");
                            emit!(finish_set(out, set, true, totals));
                            stop!(Reason::Soft);
                        }
                    }
                    continue;
                }
                let cells: Vec<Value> = row.into_iter().map(|d| value::decode(d, lob)).collect();
                set.buf.push(cells);
                set.count += 1;
                if set.buf.len() >= p.batch_size {
                    let rows = std::mem::take(&mut set.buf);
                    emit!(out.rows(rows));
                }
            }
            QueryItem::Info(m) => {
                if reason.is_none() {
                    emit!(out.message(message_text(&m)));
                }
            }
            QueryItem::Done(d) => {
                if d.attention {
                    continue;
                }
                if reason.is_some() {
                    open = None;
                    continue;
                }
                let elapsed_from = totals.last_end;
                match open.take() {
                    Some(set) => {
                        emit!(finish_set(out, set, false, totals));
                    }
                    None => {
                        // The server ends every statement with one DONE: with a row count it
                        // is `Affected`, without one (DDL, `SET`, `PRINT`, ...) it is `Done`.
                        let outcome = match (d.kind, d.error) {
                            (DoneKind::Proc, _) | (_, true) => None,
                            (_, false) if d.has_count => Some(EndOutcome::Affected { rows: d.rows }),
                            _ => Some(EndOutcome::Done),
                        };
                        if let Some(outcome) = outcome {
                            emit!(out.start(StatementStart {
                                index: totals.statements,
                                range,
                            }));
                            let end = StatementEnd {
                                outcome,
                                elapsed_ms: elapsed_from.elapsed().as_millis() as u64,
                            };
                            emit!(out.end(end));
                            totals.statements += 1;
                            totals.last_end = Instant::now();
                        }
                    }
                }
            }
        }
    }
    drop(stream);
    if let Some(se) = sink_error {
        return Err(se);
    }
    Ok(match reason {
        None => BatchEnd::Completed,
        Some(r) => BatchEnd::Stopped(Some(r)),
    })
}

/// Flush the buffered rows of `set` and end its statement.
async fn finish_set(
    out: &mut dyn Out,
    mut set: OpenSet,
    truncated: bool,
    totals: &mut Totals,
) -> Result<()> {
    if !set.buf.is_empty() {
        out.rows(std::mem::take(&mut set.buf)).await?;
    }
    out.end(StatementEnd {
        outcome: EndOutcome::Rows {
            row_count: set.count,
            truncated,
        },
        elapsed_ms: set.started.elapsed().as_millis() as u64,
    })
    .await?;
    totals.statements += 1;
    totals.last_end = Instant::now();
    Ok(())
}
