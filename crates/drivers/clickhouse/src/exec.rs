//! Statement execution shared by the session and its leases: script splitting, streaming of
//! result sets into a sink, `max_rows` / pending results, cancellation and timeouts.
//!
//! Cancellation: [`Registry::cancel`] flags the running call (the streaming loop stops at once
//! and answers `Cancelled`) and asks the server to `KILL QUERY` it. A timeout does the same and
//! answers `Timeout`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dbstudio_core::{
    EndOutcome, Error, ExecSummary, QueryRequest, Result, ResultSink, ScriptDialect,
    StatementEnd, Value,
};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::conn::{Core, Opened, Req, Source};

pub(crate) const DEFAULT_BATCH: usize = 1000;
/// Rows reach the sink at least this often even when a batch is not full yet.
const FLUSH_EVERY: Duration = Duration::from_millis(150);

/// Control block of one running call.
#[derive(Debug)]
pub(crate) struct Ctl {
    id: Uuid,
    cancelled: AtomicBool,
    timed_out: AtomicBool,
    notify: Notify,
}

impl Ctl {
    fn new(id: Uuid) -> Self {
        Ctl {
            id,
            cancelled: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub(crate) async fn cancelled_pub(&self) {
        self.cancelled().await
    }

    async fn cancelled(&self) {
        loop {
            let waiting = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            waiting.await;
        }
    }
}

/// Running calls of one session or lease, by query id.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    running: Mutex<HashMap<Uuid, Arc<Ctl>>>,
}

/// Removes the call from the registry when dropped.
pub(crate) struct Guard<'a> {
    reg: &'a Registry,
    pub ctl: Arc<Ctl>,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.reg
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.ctl.id);
    }
}

impl Registry {
    pub(crate) fn register(&self, id: Uuid) -> Guard<'_> {
        let ctl = Arc::new(Ctl::new(id));
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, ctl.clone());
        Guard { reg: self, ctl }
    }

    fn get(&self, id: Uuid) -> Option<Arc<Ctl>> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
    }

    fn all(&self) -> Vec<Arc<Ctl>> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Cancel call `id` (flag + server kill). `NotFound` when it is not running here.
    pub(crate) async fn cancel(&self, core: &Core, id: Uuid) -> Result<()> {
        let ctl = self
            .get(id)
            .ok_or_else(|| Error::NotFound("no such running query".into()))?;
        ctl.cancel();
        let _ = core.kill(&id.to_string()).await;
        Ok(())
    }

    /// Cancel whatever runs (lease `cancel()` without an id).
    pub(crate) async fn cancel_all(&self, core: &Core) {
        for ctl in self.all() {
            ctl.cancel();
            let _ = core.kill(&ctl.id.to_string()).await;
        }
    }
}

/// A result set left open after `max_rows` (lease `keep_open`).
#[derive(Debug)]
pub(crate) struct Pending {
    pub src: Source,
    /// A row already read to learn that more rows exist.
    pub row: Option<Vec<Value>>,
    pub query_id: Uuid,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamOpts {
    pub batch: usize,
    pub max_rows: Option<u64>,
}

impl StreamOpts {
    pub(crate) fn of(req: &QueryRequest) -> Self {
        StreamOpts {
            batch: req.batch_size.map_or(DEFAULT_BATCH, |b| b.max(1) as usize),
            max_rows: req.max_rows,
        }
    }
}

fn elapsed_ms(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Statements of `text` (via the ClickHouse splitter of `dbstudio-sql`).
pub(crate) fn statements(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    dbstudio_sql::split(ScriptDialect::ClickHouse, text)
        .into_iter()
        .filter_map(|r| {
            let (s, e) = (r.start as usize, (r.end as usize).min(chars.len()));
            (s < e).then(|| chars[s..e].iter().collect::<String>())
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Database named by a `USE db` statement.
pub(crate) fn parse_use(stmt: &str) -> Option<String> {
    let t = stmt.trim().trim_end_matches(';').trim();
    let head = t.get(..3)?;
    if !head.eq_ignore_ascii_case("use") {
        return None;
    }
    let rest = t[3..].trim();
    if rest.is_empty() || !t[3..].starts_with(char::is_whitespace) {
        return None;
    }
    let unquoted = match rest.chars().next()? {
        q @ ('`' | '"') if rest.ends_with(q) && rest.len() >= 2 => {
            rest[1..rest.len() - 1].replace(&format!("{q}{q}"), &q.to_string())
        }
        _ => rest.to_string(),
    };
    (!unquoted.contains(char::is_whitespace) || rest.starts_with(['`', '"'])).then_some(unquoted)
}

/// Everything a call needs besides the sink.
pub(crate) struct Call<'a> {
    pub core: &'a Arc<Core>,
    pub ctl: &'a Ctl,
    pub session_id: Option<&'a str>,
    pub timeout: Option<Duration>,
    pub started: Instant,
}

impl Call<'_> {
    fn req(&self, db: &Option<String>, id: Uuid) -> Req {
        Req {
            database: db.clone(),
            query_id: Some(id.to_string()),
            session_id: self.session_id.map(str::to_string),
            timeout_ms: self
                .timeout
                .map(|t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX)),
            settings: Vec::new(),
        }
    }

    fn deadline(&self) -> Option<Instant> {
        // The server enforces `max_execution_time`; the client waits a little longer.
        self.timeout.map(|t| self.started + t + Duration::from_secs(2))
    }

    /// Stop the server-side query too. A call inside an HTTP session waits for the kill (the
    /// session stays locked while the query runs and the next statement needs it).
    async fn abort(&self, id: Uuid) {
        let core = self.core.clone();
        if self.session_id.is_some() {
            let _ = tokio::time::timeout(Duration::from_secs(5), core.kill_sync(&id.to_string()))
                .await;
        } else {
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(5), core.kill(&id.to_string()))
                    .await;
            });
        }
    }
}

/// Wait for the next row, stopping at cancel or deadline.
async fn next_row(call: &Call<'_>, src: &mut Source, id: Uuid) -> Result<Option<Vec<Value>>> {
    let deadline = call.deadline();
    let sleep = tokio::time::sleep_until(
        deadline
            .map(tokio::time::Instant::from_std)
            .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(86_400 * 365)),
    );
    tokio::pin!(sleep);
    let r = tokio::select! {
        r = src.next_row() => r,
        () = call.ctl.cancelled() => Err(Error::Cancelled),
        () = &mut sleep, if deadline.is_some() => {
            call.ctl.timed_out.store(true, Ordering::SeqCst);
            Err(Error::Timeout)
        }
    };
    if matches!(r, Err(Error::Cancelled) | Err(Error::Timeout)) {
        call.abort(id).await;
    }
    r
}

/// Result of streaming one result set.
pub(crate) struct Streamed {
    pub count: u64,
    pub truncated: bool,
    /// The row read beyond `max_rows`.
    pub extra: Option<Vec<Value>>,
}

/// Push rows of `src` to `sink` in batches until the end or `max_rows`.
pub(crate) async fn stream_rows(
    call: &Call<'_>,
    src: &mut Source,
    first: Option<Vec<Value>>,
    id: Uuid,
    o: &StreamOpts,
    sink: &mut dyn ResultSink,
) -> Result<Streamed> {
    let mut buf: Vec<Vec<Value>> = Vec::with_capacity(o.batch.min(4096));
    let mut count = 0u64;
    let mut next = first;
    let mut last_flush = Instant::now();
    let mut extra = None;
    let mut truncated = false;
    loop {
        let row = match next.take() {
            Some(r) => Some(r),
            None => next_row(call, src, id).await?,
        };
        let Some(row) = row else { break };
        if o.max_rows.is_some_and(|m| count >= m) {
            truncated = true;
            extra = Some(row);
            break;
        }
        buf.push(row);
        count += 1;
        if buf.len() >= o.batch || last_flush.elapsed() >= FLUSH_EVERY {
            if let Err(e) = sink.on_rows(std::mem::take(&mut buf)) {
                call.abort(id).await;
                return Err(e);
            }
            last_flush = Instant::now();
        }
    }
    if !buf.is_empty() {
        if let Err(e) = sink.on_rows(buf) {
            call.abort(id).await;
            return Err(e);
        }
    }
    Ok(Streamed {
        count,
        truncated,
        extra,
    })
}

/// Run a script. `db` is the current database (updated by `USE`). With `keep` the last result
/// set stays open in `pending` when it hit `max_rows`.
pub(crate) async fn run_script(
    call: &Call<'_>,
    db: &mut Option<String>,
    req: &QueryRequest,
    keep: Option<&mut Option<Pending>>,
    sink: &mut dyn ResultSink,
) -> Result<ExecSummary> {
    let started = Instant::now();
    if let Some(d) = req.database.as_deref().filter(|d| !d.is_empty()) {
        *db = Some(d.to_string());
    }
    let stmts = statements(&req.text);
    let o = StreamOpts::of(req);
    let n = stmts.len();
    let mut keep = keep;
    let mut done = 0u32;
    for (i, stmt) in stmts.iter().enumerate() {
        if call.ctl.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let t0 = Instant::now();
        if let Some(target) = parse_use(stmt) {
            // `USE` is client state: the database travels with every request.
            let exists = call
                .core
                .rows(
                    &Req::default(),
                    &format!(
                        "SELECT count() FROM system.databases WHERE name = {}",
                        crate::conn::quote_literal(&target)
                    ),
                )
                .await?
                .first()
                .and_then(|r| crate::conn::cell_u64(r, 0))
                .unwrap_or(0);
            if exists == 0 {
                return Err(Error::Query {
                    message: format!("Database {target} does not exist"),
                    position: None,
                });
            }
            *db = Some(target);
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Done,
                elapsed_ms: elapsed_ms(t0),
            })?;
            done += 1;
            continue;
        }
        let id = req.query_id;
        let opened = Source::open(call.core, &call.req(db, id), stmt).await;
        let opened = match opened {
            Ok(o) => o,
            Err(e) => return Err(finish_error(call, e)),
        };
        match opened {
            Opened::NoResult => {
                sink.on_statement_end(StatementEnd {
                    outcome: EndOutcome::Done,
                    elapsed_ms: elapsed_ms(t0),
                })?;
            }
            Opened::Rows(mut src) => {
                sink.on_columns(src.metas())?;
                let streamed = stream_rows(call, &mut src, None, id, &o, sink)
                    .await
                    .map_err(|e| finish_error(call, e))?;
                let last = i + 1 == n;
                sink.on_statement_end(StatementEnd {
                    outcome: EndOutcome::Rows {
                        row_count: streamed.count,
                        truncated: streamed.truncated,
                    },
                    elapsed_ms: elapsed_ms(t0),
                })?;
                if streamed.truncated {
                    match keep.as_deref_mut() {
                        Some(slot) if last => {
                            *slot = Some(Pending {
                                src,
                                row: streamed.extra,
                                query_id: id,
                            });
                        }
                        _ => call.abort(id).await,
                    }
                }
            }
        }
        done += 1;
    }
    Ok(ExecSummary {
        statements: done,
        elapsed_ms: elapsed_ms(started),
    })
}

/// Give timeouts and cancellations the error they deserve.
pub(crate) fn finish_error(call: &Call<'_>, e: Error) -> Error {
    if call.ctl.timed_out.load(Ordering::SeqCst) {
        Error::Timeout
    } else if call.ctl.is_cancelled() {
        Error::Cancelled
    } else {
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_statements_are_recognised() {
        assert_eq!(parse_use("USE analytics"), Some("analytics".into()));
        assert_eq!(parse_use("use `my db`;"), Some("my db".into()));
        assert_eq!(parse_use("  Use \"x\"  "), Some("x".into()));
        assert_eq!(parse_use("USEFUL"), None);
        assert_eq!(parse_use("SELECT 1"), None);
        assert_eq!(parse_use("use"), None);
    }

    #[test]
    fn registry_tracks_calls() {
        let reg = Registry::default();
        let id = Uuid::new_v4();
        {
            let g = reg.register(id);
            assert!(reg.get(id).is_some());
            assert!(!g.ctl.is_cancelled());
        }
        assert!(reg.get(id).is_none());
    }
}
