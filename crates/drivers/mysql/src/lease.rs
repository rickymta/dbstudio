//! MySQL/MariaDB driver: dedicated connection (lease) for editor tabs, built on
//! [`dbstudio_core::lease_kit`].
//!
//! * The connection is opened outside the pool (`Conn::new` with the session's options, so TLS,
//!   `SET NAMES` and read-only setup apply) and owned by one actor task.
//! * Cancelling runs `KILL QUERY <id>` on a short-lived second connection. A per-statement gate
//!   guarantees that a late `KILL` never hits a later statement.
//! * The server does not report its transaction state to the client in a form `mysql_async`
//!   exposes, so the state is tracked from the statements that ran (`START TRANSACTION`,
//!   `COMMIT`, `ROLLBACK`, implicit commits of DDL, and in manual mode any data statement).
//! * A script is split into statements (see [`split_statements`]) so that each one can be
//!   reported with its range and so only the last one needs to stay open.
//! * `fetch_more`: `mysql_async` result streams borrow the connection, so when the last
//!   statement of a request may stay open it runs on a *parked task* that owns the connection
//!   and the stream; after the result hit `max_rows` the task waits for `fetch_more` or
//!   `discard_result`, then hands the connection back.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dbstudio_core::lease_kit::{ActorLease, Canceller, LeaseBackend, LeaseEvent};
use dbstudio_core::{
    ColumnMeta, EndOutcome, Error, ExecSummary, IsolationLevel, Lease, LeaseContext, LeaseOptions,
    QueryRequest, Result, ResultSink, StatementEnd, StatementStart, TextRange, TxControl,
    TxOptions, TxStatus, Value,
};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, Row};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::connect::DEFAULT_CONNECT_TIMEOUT;
use crate::error::{map_connect_error, map_query_error};
use crate::session::MySqlSession;
use crate::sql::quote_ident;
use crate::value::{ColInfo, decode_cell};

const DEFAULT_BATCH: usize = 1000;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const KILL_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Open a dedicated connection.
pub(crate) async fn open(session: &MySqlSession, opts: LeaseOptions) -> Result<Box<dyn Lease>> {
    let inner = &session.inner;
    let connect = Conn::new(inner.opts.clone());
    let mut conn = match tokio::time::timeout(DEFAULT_CONNECT_TIMEOUT, connect).await {
        Err(_) => return Err(Error::Timeout),
        Ok(Err(e)) => return Err(map_connect_error(e, None)),
        Ok(Ok(c)) => c,
    };
    if let Some(db) = opts.database.as_deref().filter(|d| !d.is_empty()) {
        conn.query_drop(format!("USE {}", quote_ident(db)))
            .await
            .map_err(map_query_error)?;
    }
    if !opts.auto_commit {
        conn.query_drop("SET autocommit=0")
            .await
            .map_err(map_query_error)?;
    }
    let database: Option<String> = conn
        .query_first("SELECT DATABASE()")
        .await
        .map_err(map_query_error)?;
    let conn_id = conn.id();
    let backend = MyBackend {
        killer: Arc::new(Killer {
            opts: inner.opts.clone(),
            conn_id,
        }),
        backslash_escapes: inner.backslash_escapes,
        conn: Some(conn),
        auto_commit: opts.auto_commit,
        in_tx: false,
        parked: None,
        current: Arc::new(Mutex::new(None)),
    };
    let initial = LeaseOptions {
        database,
        schema: None,
        auto_commit: opts.auto_commit,
    };
    Ok(Box::new(ActorLease::spawn(backend, initial)?))
}

// ------------------------------------------------------------------------------------------
// Statement splitting and classification

/// One statement of a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Piece {
    /// Text to send (no terminating `;`).
    pub text: String,
    /// Character range in the script, without surrounding whitespace.
    pub range: (usize, usize),
}

/// Split a script at top-level semicolons, honouring quotes (`'`, `"`, `` ` ``) and comments
/// (`-- `, `#`, `/* */`). Pieces made of whitespace and comments only are dropped. The client
/// command `DELIMITER` is not supported.
pub(crate) fn split_statements(script: &str, backslash_escapes: bool) -> Vec<Piece> {
    let chars: Vec<char> = script.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut seg_start = 0usize;
    let mut has_code = false;
    let mut i = 0usize;
    let flush = |from: usize, to: usize, has_code: bool, out: &mut Vec<Piece>| {
        if !has_code {
            return;
        }
        let mut s = from;
        let mut e = to;
        while s < e && chars[s].is_whitespace() {
            s += 1;
        }
        while e > s && chars[e - 1].is_whitespace() {
            e -= 1;
        }
        out.push(Piece {
            text: chars[s..e].iter().collect(),
            range: (s, e),
        });
    };
    while i < n {
        let c = chars[i];
        match c {
            '\'' | '"' | '`' => {
                has_code = true;
                i += 1;
                while i < n {
                    if chars[i] == '\\' && backslash_escapes && c != '`' {
                        i += 2;
                        continue;
                    }
                    if chars[i] == c {
                        if i + 1 < n && chars[i + 1] == c {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            '-' if i + 1 < n
                && chars[i + 1] == '-'
                && (i + 2 >= n || chars[i + 2].is_whitespace() || chars[i + 2].is_control()) =>
            {
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
            }
            '#' => {
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if i + 1 < n && chars[i + 1] == '*' => {
                // `/*! ... */` and `/*+ ... */` carry code or hints: they count as code.
                if i + 2 < n && (chars[i + 2] == '!' || chars[i + 2] == '+') {
                    has_code = true;
                }
                i += 2;
                while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            ';' => {
                flush(seg_start, i, has_code, &mut out);
                seg_start = i + 1;
                has_code = false;
                i += 1;
            }
            c if c.is_whitespace() => i += 1,
            _ => {
                has_code = true;
                i += 1;
            }
        }
    }
    flush(seg_start, n.min(chars.len()), has_code, &mut out);
    out
}

/// Upper-cased words at the start of a statement (comments skipped).
fn leading_words(stmt: &str, count: usize) -> Vec<String> {
    let mut words = Vec::new();
    let mut rest = stmt.trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("/*") {
            match r.find("*/") {
                Some(p) => {
                    rest = r[p + 2..].trim_start();
                    continue;
                }
                None => break,
            }
        }
        if rest.starts_with('#') || rest.starts_with("-- ") {
            match rest.find('\n') {
                Some(p) => {
                    rest = rest[p + 1..].trim_start();
                    continue;
                }
                None => break,
            }
        }
        break;
    }
    for w in rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if w.is_empty() {
            continue;
        }
        words.push(w.to_ascii_uppercase());
        if words.len() >= count {
            break;
        }
    }
    words
}

/// What a statement does to the open transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxEffect {
    /// Starts a transaction (`START TRANSACTION`, `BEGIN`).
    Begin,
    /// Ends it (`COMMIT`, `ROLLBACK`, an implicit commit).
    End,
    /// A statement that opens a transaction when autocommit is off.
    Data,
    /// No effect.
    None,
}

fn tx_effect(stmt: &str) -> TxEffect {
    let w = leading_words(stmt, 2);
    let first = w.first().map(String::as_str).unwrap_or("");
    let second = w.get(1).map(String::as_str).unwrap_or("");
    match first {
        "START" | "BEGIN" => TxEffect::Begin,
        "COMMIT" => TxEffect::End,
        "ROLLBACK" => {
            if second == "TO" || second == "SAVEPOINT" {
                TxEffect::None
            } else {
                TxEffect::End
            }
        }
        // Statements that commit implicitly.
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "LOCK" | "UNLOCK" | "GRANT"
        | "REVOKE" | "ANALYZE" | "OPTIMIZE" | "REPAIR" | "FLUSH" | "INSTALL" | "UNINSTALL" => {
            TxEffect::End
        }
        "SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "CALL" | "LOAD"
        | "TABLE" | "VALUES" | "DO" | "HANDLER" => TxEffect::Data,
        "SET" if second == "AUTOCOMMIT" => TxEffect::None,
        _ => TxEffect::None,
    }
}

fn starts_with_use(stmt: &str) -> bool {
    leading_words(stmt, 1).first().is_some_and(|w| w == "USE")
}

// ------------------------------------------------------------------------------------------
// Cancellation

struct Killer {
    opts: Opts,
    conn_id: u32,
}

impl Killer {
    /// `KILL QUERY` through a dedicated short-lived connection (never the pool, which may be
    /// exhausted by the very statements we want to stop).
    async fn kill_query(&self) {
        let opts = self.opts.clone();
        let id = self.conn_id;
        let work = async move {
            let mut c = Conn::new(opts).await?;
            c.query_drop(format!("KILL QUERY {id}")).await?;
            c.disconnect().await
        };
        match tokio::time::timeout(KILL_TIMEOUT, work).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("KILL QUERY {id} failed: {e}"),
            Err(_) => tracing::warn!("KILL QUERY {id} timed out"),
        }
    }
}

/// Control block of one running statement.
#[derive(Default)]
struct CallCtl {
    cancelled: AtomicBool,
    /// `true` once the statement ended or was killed (gate against a late `KILL`).
    gate: tokio::sync::Mutex<GateState>,
}

#[derive(Default)]
struct GateState {
    finished: bool,
    killed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KillReason {
    /// The caller asked to cancel.
    Cancel,
    /// Internal stop (sink refused data, result discarded).
    Abort,
}

impl CallCtl {
    fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    async fn kill(&self, killer: &Killer, reason: KillReason) {
        let mut g = self.gate.lock().await;
        if g.finished || g.killed {
            return;
        }
        if reason == KillReason::Cancel {
            self.cancelled.store(true, Ordering::SeqCst);
        }
        g.killed = true;
        killer.kill_query().await;
    }

    async fn finish(&self) {
        self.gate.lock().await.finished = true;
    }

    fn finalize<T>(&self, res: Result<T>) -> Result<T> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err(Error::Cancelled)
        } else {
            res
        }
    }
}

type CurrentCall = Arc<Mutex<Option<Arc<CallCtl>>>>;

struct MyCanceller {
    killer: Arc<Killer>,
    current: CurrentCall,
}

#[async_trait]
impl Canceller for MyCanceller {
    async fn cancel(&self) -> Result<()> {
        let ctl = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(ctl) = ctl {
            ctl.kill(&self.killer, KillReason::Cancel).await;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------
// Row decoding

fn value_text(v: &mysql_async::Value) -> Option<String> {
    use mysql_async::Value as V;
    Some(match v {
        V::NULL => return None,
        V::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        V::Int(i) => i.to_string(),
        V::UInt(u) => u.to_string(),
        V::Float(f) => f.to_string(),
        V::Double(d) => d.to_string(),
        V::Date(y, mo, d, h, mi, s, us) => {
            if *us == 0 {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
            } else {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}")
            }
        }
        V::Time(neg, days, h, mi, s, us) => {
            let hours = u64::from(*days) * 24 + u64::from(*h);
            let sign = if *neg { "-" } else { "" };
            if *us == 0 {
                format!("{sign}{hours:02}:{mi:02}:{s:02}")
            } else {
                format!("{sign}{hours:02}:{mi:02}:{s:02}.{us:06}")
            }
        }
    })
}

fn decode_value(info: &ColInfo, v: mysql_async::Value) -> Value {
    use mysql_async::Value as V;
    match v {
        V::NULL => Value::Null,
        V::Bytes(b) => decode_cell(info, b),
        V::Int(i) => Value::Int(i),
        V::UInt(u) => Value::UInt(u),
        V::Float(f) => Value::Float(f64::from(f)),
        V::Double(d) => Value::Float(d),
        other => value_text(&other).map(Value::Text).unwrap_or(Value::Null),
    }
}

fn decode_row(infos: &[ColInfo], row: Row) -> Vec<Value> {
    row.unwrap()
        .into_iter()
        .enumerate()
        .map(|(i, v)| match infos.get(i) {
            Some(info) => decode_value(info, v),
            None => value_text(&v).map(Value::Text).unwrap_or(Value::Null),
        })
        .collect()
}

fn elapsed_ms(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// ------------------------------------------------------------------------------------------
// Parked statements

/// Where the events of a parked statement go; switched by every `fetch_more`.
#[derive(Clone)]
struct Target {
    inner: Arc<Mutex<(mpsc::UnboundedSender<LeaseEvent>, Arc<AtomicBool>)>>,
}

impl Target {
    fn new(tx: mpsc::UnboundedSender<LeaseEvent>, stop: Arc<AtomicBool>) -> Self {
        Target {
            inner: Arc::new(Mutex::new((tx, stop))),
        }
    }
    fn set(&self, tx: mpsc::UnboundedSender<LeaseEvent>, stop: Arc<AtomicBool>) {
        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = (tx, stop);
    }
    fn send(&self, event: LeaseEvent) -> Result<()> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.1.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        g.0.send(event).map_err(|_| Error::Cancelled)
    }
}

/// Sink of a parked statement: forwards to the current [`Target`].
struct SwitchSink(Target);

impl ResultSink for SwitchSink {
    fn on_columns(&mut self, columns: Vec<ColumnMeta>) -> Result<()> {
        self.0.send(LeaseEvent::Columns(columns))
    }
    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        self.0.send(LeaseEvent::Rows(rows))
    }
    fn on_message(&mut self, message: String) -> Result<()> {
        self.0.send(LeaseEvent::Message(message))
    }
    fn on_statement_end(&mut self, end: StatementEnd) -> Result<()> {
        self.0.send(LeaseEvent::End(end))
    }
    fn on_statement_start(&mut self, start: StatementStart) -> Result<()> {
        self.0.send(LeaseEvent::Start(start))
    }
}

/// Progress report of a parked statement.
enum Status {
    /// The result hit `max_rows` and waits for `fetch_more` / `discard`.
    Parked,
    /// The statement ended.
    Finished(Result<()>),
}

/// One call (`execute` or `fetch_more`) on a parked statement.
struct Call {
    events: mpsc::UnboundedSender<LeaseEvent>,
    stop: Arc<AtomicBool>,
    status: mpsc::UnboundedSender<Status>,
}

enum ParkCmd {
    Fetch {
        max_rows: u64,
        batch: usize,
        call: Call,
    },
    Discard,
}

enum Resume {
    Fetch { max_rows: u64, batch: usize },
    Discard,
}

/// The parking side of a statement, used inside [`stream_statement`].
struct Parker {
    cmds: mpsc::UnboundedReceiver<ParkCmd>,
    status: mpsc::UnboundedSender<Status>,
    target: Target,
}

impl Parker {
    async fn park(&mut self) -> Resume {
        let _ = self.status.send(Status::Parked);
        match self.cmds.recv().await {
            Some(ParkCmd::Fetch {
                max_rows,
                batch,
                call,
            }) => {
                self.target.set(call.events, call.stop);
                self.status = call.status;
                Resume::Fetch { max_rows, batch }
            }
            Some(ParkCmd::Discard) | None => Resume::Discard,
        }
    }
}

struct StreamOpts {
    batch: usize,
    max_rows: Option<u64>,
}

/// Run one statement and stream every result set into `sink`. With a `parker`, a result set
/// that hits `max_rows` pauses until the parker is resumed. Returns the result plus whether the
/// connection must be discarded.
#[allow(clippy::too_many_arguments)]
async fn stream_statement(
    killer: &Killer,
    conn: &mut Conn,
    ctl: &CallCtl,
    sql: &str,
    mut o: StreamOpts,
    sink: &mut dyn ResultSink,
    statements: &mut u32,
    mut parker: Option<&mut Parker>,
) -> (Result<()>, bool) {
    let mut result = match conn.query_iter(sql).await {
        Ok(r) => r,
        Err(e) => return (Err(map_query_error(e)), false),
    };
    let mut failure: Option<Error> = None;
    // True when `failure` is an error the server itself reported (nothing left to kill).
    let mut server_failed = false;
    // The result was dropped on purpose (`discard_result`): not an error.
    let mut discarded = false;
    let mut started = Instant::now();

    'sets: while let Some(cols) = result.columns() {
        if ctl.stopped() {
            failure = Some(Error::Cancelled);
            break;
        }

        if cols.is_empty() {
            // A statement without a result set (DML, DDL, SET, ...).
            let affected = result.affected_rows();
            let warnings = result.warnings();
            let touched = affected > 0
                || result.last_insert_id().unwrap_or(0) > 0
                || !result.info().is_empty();
            let next = result.next().await;
            if warnings > 0 {
                if let Err(e) = sink.on_message(format!("{warnings} warning(s)")) {
                    failure = Some(e);
                    break;
                }
            }
            *statements += 1;
            let outcome = if touched {
                EndOutcome::Affected { rows: affected }
            } else {
                EndOutcome::Done
            };
            let end = StatementEnd {
                outcome,
                elapsed_ms: elapsed_ms(started),
            };
            started = Instant::now();
            if let Err(e) = sink.on_statement_end(end) {
                failure = Some(e);
                break;
            }
            if let Err(e) = next {
                failure = Some(map_query_error(e));
                server_failed = true;
                break;
            }
            continue;
        }

        let infos: Vec<ColInfo> = cols.iter().map(ColInfo::of).collect();
        let metas: Vec<ColumnMeta> = cols
            .iter()
            .zip(&infos)
            .map(|(c, i)| ColumnMeta {
                name: c.name_str().into_owned(),
                type_name: i.type_name(),
                value_kind: i.value_kind(),
                nullable: Some(i.nullable),
            })
            .collect();
        if let Err(e) = sink.on_columns(metas) {
            failure = Some(e);
            break;
        }

        let mut buf: Vec<Vec<Value>> = Vec::with_capacity(o.batch.min(4096));
        let mut count = 0u64;
        let mut truncated = false;
        let mut counted = false;
        let mut row_error: Option<Error> = None;
        // The row that proved there is more than `max_rows`, kept for the next fetch.
        let mut lookahead: Option<Row> = None;
        loop {
            let next = match lookahead.take() {
                Some(row) => Ok(Some(row)),
                None => result.next().await,
            };
            match next {
                Ok(Some(row)) => {
                    if ctl.stopped() {
                        failure = Some(Error::Cancelled);
                        break 'sets;
                    }
                    if o.max_rows.is_some_and(|m| count >= m) {
                        let Some(p) = parker.as_deref_mut() else {
                            truncated = true; // discard the rest of this result set
                            continue;
                        };
                        // Deliver what we have, report the cut and wait.
                        if !buf.is_empty() {
                            if let Err(e) = sink.on_rows(std::mem::take(&mut buf)) {
                                failure = Some(e);
                                break 'sets;
                            }
                        }
                        if !counted {
                            *statements += 1;
                            counted = true;
                        }
                        let end = StatementEnd {
                            outcome: EndOutcome::Rows {
                                row_count: count,
                                truncated: true,
                            },
                            elapsed_ms: elapsed_ms(started),
                        };
                        if let Err(e) = sink.on_statement_end(end) {
                            failure = Some(e);
                            break 'sets;
                        }
                        lookahead = Some(row);
                        match p.park().await {
                            Resume::Fetch { max_rows, batch } => {
                                o.max_rows = Some(max_rows);
                                o.batch = batch;
                                count = 0;
                                started = Instant::now();
                                continue;
                            }
                            Resume::Discard => {
                                discarded = true;
                                break 'sets;
                            }
                        }
                    }
                    buf.push(decode_row(&infos, row));
                    count += 1;
                    if buf.len() >= o.batch {
                        if let Err(e) = sink.on_rows(std::mem::take(&mut buf)) {
                            failure = Some(e);
                            break 'sets;
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    row_error = Some(map_query_error(e));
                    break;
                }
            }
        }
        if let Some(e) = row_error {
            // If more results were announced, this set was complete and the error belongs to
            // the next statement: still deliver this one.
            if !result.is_empty() && !ctl.stopped() {
                if !buf.is_empty() {
                    let _ = sink.on_rows(std::mem::take(&mut buf));
                }
                if !counted {
                    *statements += 1;
                }
                let _ = sink.on_statement_end(StatementEnd {
                    outcome: EndOutcome::Rows {
                        row_count: count,
                        truncated,
                    },
                    elapsed_ms: elapsed_ms(started),
                });
            }
            failure = Some(e);
            server_failed = true;
            break;
        }
        if !buf.is_empty() {
            if let Err(e) = sink.on_rows(std::mem::take(&mut buf)) {
                failure = Some(e);
                break;
            }
        }
        if !counted {
            *statements += 1;
        }
        let end = StatementEnd {
            outcome: EndOutcome::Rows {
                row_count: count,
                truncated,
            },
            elapsed_ms: elapsed_ms(started),
        };
        started = Instant::now();
        if let Err(e) = sink.on_statement_end(end) {
            failure = Some(e);
            break;
        }
    }

    // Leave the connection in a clean protocol state.
    let mut broken = false;
    if (failure.is_some() || discarded) && !server_failed {
        // Stop the server from producing more data (no-op when it already finished).
        ctl.kill(killer, KillReason::Abort).await;
    }
    match tokio::time::timeout(DRAIN_TIMEOUT, result.drop_result()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            if e.is_fatal() {
                broken = true;
            }
            // An error announced by the server for a later statement surfaces only now; when we
            // are already failing (kill / sink error) it is just the echo of our own stop.
            if failure.is_none() && !discarded {
                failure = Some(map_query_error(e));
            }
        }
        Err(_) => broken = true,
    }
    match failure {
        Some(e) => (Err(e), broken),
        None => (Ok(()), broken),
    }
}

/// Body of the task that owns the connection while the last statement may stay open.
async fn parked_task(
    killer: Arc<Killer>,
    mut conn: Conn,
    ctl: Arc<CallCtl>,
    sql: String,
    o: StreamOpts,
    mut parker: Parker,
) -> (Conn, bool) {
    let mut sink = SwitchSink(parker.target.clone());
    let mut statements = 0u32;
    let (res, broken) = stream_statement(
        &killer,
        &mut conn,
        &ctl,
        &sql,
        o,
        &mut sink,
        &mut statements,
        Some(&mut parker),
    )
    .await;
    ctl.finish().await;
    let _ = parker.status.send(Status::Finished(ctl.finalize(res)));
    (conn, broken)
}

/// A statement waiting on its parked task.
struct Parked {
    cmds: mpsc::UnboundedSender<ParkCmd>,
    task: JoinHandle<(Conn, bool)>,
    ctl: Arc<CallCtl>,
}

// ------------------------------------------------------------------------------------------
// Backend

struct MyBackend {
    killer: Arc<Killer>,
    backslash_escapes: bool,
    /// `None` while a parked task owns it, or after the connection was lost / closed.
    conn: Option<Conn>,
    auto_commit: bool,
    in_tx: bool,
    parked: Option<Parked>,
    current: CurrentCall,
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
                "snapshot isolation on MySQL / MariaDB".into(),
            ));
        }
    })
}

impl MyBackend {
    fn conn(&mut self) -> Result<&mut Conn> {
        self.conn.as_mut().ok_or_else(lost)
    }

    fn set_current(&self, ctl: Option<Arc<CallCtl>>) {
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = ctl;
    }

    fn apply_effect(&mut self, stmt: &str) {
        match tx_effect(stmt) {
            TxEffect::Begin => self.in_tx = true,
            TxEffect::End => self.in_tx = false,
            TxEffect::Data => {
                if !self.auto_commit {
                    self.in_tx = true;
                }
            }
            TxEffect::None => {}
        }
    }

    /// Take the connection back from a parked task.
    async fn reclaim(&mut self, parked: Parked) {
        match tokio::time::timeout(DRAIN_TIMEOUT + KILL_TIMEOUT, parked.task).await {
            Ok(Ok((conn, broken))) => {
                if broken {
                    let _ = conn.disconnect().await;
                } else {
                    self.conn = Some(conn);
                }
            }
            // The task panicked or never ended: the connection is gone.
            _ => self.conn = None,
        }
        self.set_current(None);
    }

    /// Forward the events of a parked statement to `sink` until it parks again or ends.
    async fn relay(
        &self,
        mut events: mpsc::UnboundedReceiver<LeaseEvent>,
        mut status: mpsc::UnboundedReceiver<Status>,
        stop: &AtomicBool,
        ctl: &CallCtl,
        sink: &mut dyn ResultSink,
    ) -> (Status, Option<Error>) {
        let mut sink_error: Option<Error> = None;
        let mut events_open = true;
        let final_status = loop {
            tokio::select! {
                biased;
                event = events.recv(), if events_open => match event {
                    Some(event) => {
                        if sink_error.is_none() {
                            if let Err(e) = event.deliver(sink) {
                                sink_error = Some(e);
                                stop.store(true, Ordering::Release);
                                ctl.kill(&self.killer, KillReason::Abort).await;
                            }
                        }
                    }
                    None => events_open = false,
                },
                st = status.recv() => {
                    break st.unwrap_or_else(|| Status::Finished(Err(lost())));
                }
            }
        };
        while let Ok(event) = events.try_recv() {
            if sink_error.is_none() {
                if let Err(e) = event.deliver(sink) {
                    sink_error = Some(e);
                }
            }
        }
        (final_status, sink_error)
    }

    /// Run the last statement on a parked task. Returns the statements counted.
    async fn run_parked(
        &mut self,
        piece: &Piece,
        req: &QueryRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<u32> {
        let conn = self.conn.take().ok_or_else(lost)?;
        let ctl = Arc::new(CallCtl::default());
        self.set_current(Some(ctl.clone()));
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (status_tx, status_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let parker = Parker {
            cmds: cmd_rx,
            status: status_tx,
            target: Target::new(events_tx, stop.clone()),
        };
        let o = StreamOpts {
            batch: req.batch_size.map_or(DEFAULT_BATCH, |b| b.max(1) as usize),
            max_rows: req.max_rows,
        };
        let task = tokio::spawn(parked_task(
            self.killer.clone(),
            conn,
            ctl.clone(),
            piece.text.clone(),
            o,
            parker,
        ));
        let parked = Parked {
            cmds: cmd_tx,
            task,
            ctl: ctl.clone(),
        };
        let (status, sink_error) = self.relay(events_rx, status_rx, &stop, &ctl, sink).await;
        self.settle(parked, status, sink_error).await.map(|()| 1)
    }

    /// Decide what happens to a parked statement after a call ended.
    async fn settle(
        &mut self,
        parked: Parked,
        status: Status,
        sink_error: Option<Error>,
    ) -> Result<()> {
        match status {
            Status::Parked => {
                if let Some(e) = sink_error {
                    // The consumer is gone: do not keep the result open.
                    let _ = parked.cmds.send(ParkCmd::Discard);
                    self.reclaim(parked).await;
                    return Err(e);
                }
                self.parked = Some(parked);
                Ok(())
            }
            Status::Finished(result) => {
                self.reclaim(parked).await;
                match sink_error {
                    Some(e) => Err(e),
                    None => result,
                }
            }
        }
    }

    async fn drop_pending(&mut self) {
        if let Some(parked) = self.parked.take() {
            let _ = parked.cmds.send(ParkCmd::Discard);
            self.reclaim(parked).await;
        }
    }

    async fn simple(&mut self, sql: &str) -> Result<()> {
        let conn = self.conn()?;
        match conn.query_drop(sql).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let fatal = e.is_fatal();
                let mapped = map_query_error(e);
                if fatal {
                    self.conn = None;
                    return Err(Error::Connection(mapped.to_string()));
                }
                Err(mapped)
            }
        }
    }

    async fn query_database(&mut self) -> Option<String> {
        let conn = self.conn.as_mut()?;
        conn.query_first::<Option<String>, _>("SELECT DATABASE()")
            .await
            .ok()
            .flatten()
            .flatten()
    }
}

#[async_trait]
impl LeaseBackend for MyBackend {
    async fn execute(
        &mut self,
        req: QueryRequest,
        sink: &mut dyn ResultSink,
        keep_open: bool,
    ) -> Result<ExecSummary> {
        self.drop_pending().await;
        let started = Instant::now();
        let pieces = split_statements(&req.text, self.backslash_escapes);
        let last = pieces.len().saturating_sub(1);
        let mut statements = 0u32;
        for (index, piece) in pieces.iter().enumerate() {
            sink.on_statement_start(StatementStart {
                index: index as u32,
                range: TextRange {
                    start: piece.range.0 as u32,
                    end: piece.range.1 as u32,
                },
            })?;
            if keep_open && index == last && req.max_rows.is_some() {
                statements += self.run_parked(piece, &req, sink).await?;
                self.apply_effect(&piece.text);
                continue;
            }
            let mut conn = self.conn.take().ok_or_else(lost)?;
            let ctl = Arc::new(CallCtl::default());
            self.set_current(Some(ctl.clone()));
            let o = StreamOpts {
                batch: req.batch_size.map_or(DEFAULT_BATCH, |b| b.max(1) as usize),
                max_rows: req.max_rows,
            };
            let (res, broken) = stream_statement(
                &self.killer,
                &mut conn,
                &ctl,
                &piece.text,
                o,
                sink,
                &mut statements,
                None,
            )
            .await;
            ctl.finish().await;
            self.set_current(None);
            if broken {
                let _ = conn.disconnect().await;
            } else {
                self.conn = Some(conn);
            }
            match ctl.finalize(res) {
                Ok(()) => self.apply_effect(&piece.text),
                Err(e) => {
                    return Err(if broken && !matches!(e, Error::Connection(_)) {
                        lost()
                    } else {
                        e
                    });
                }
            }
        }
        Ok(ExecSummary {
            statements,
            elapsed_ms: elapsed_ms(started),
        })
    }

    async fn fetch_more(
        &mut self,
        max_rows: u64,
        batch: usize,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let parked = self
            .parked
            .take()
            .ok_or_else(|| Error::NotFound("pending result".into()))?;
        let started = Instant::now();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (status_tx, status_rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let call = Call {
            events: events_tx,
            stop: stop.clone(),
            status: status_tx,
        };
        if parked
            .cmds
            .send(ParkCmd::Fetch {
                max_rows,
                batch,
                call,
            })
            .is_err()
        {
            self.reclaim(parked).await;
            return Err(lost());
        }
        let ctl = parked.ctl.clone();
        self.set_current(Some(ctl.clone()));
        let (status, sink_error) = self.relay(events_rx, status_rx, &stop, &ctl, sink).await;
        self.settle(parked, status, sink_error).await?;
        Ok(ExecSummary {
            statements: 1,
            elapsed_ms: elapsed_ms(started),
        })
    }

    async fn discard_result(&mut self) -> Result<()> {
        self.drop_pending().await;
        Ok(())
    }

    async fn tx(&mut self, op: TxControl, opts: TxOptions) -> Result<()> {
        self.drop_pending().await;
        match op {
            TxControl::Begin => {
                if let Some(level) = opts.isolation {
                    let sql = format!("SET TRANSACTION ISOLATION LEVEL {}", isolation_sql(level)?);
                    self.simple(&sql).await?;
                }
                let sql = if opts.read_only {
                    "START TRANSACTION READ ONLY"
                } else {
                    "START TRANSACTION"
                };
                self.simple(sql).await?;
                self.in_tx = true;
            }
            TxControl::Commit => {
                self.simple("COMMIT").await?;
                self.in_tx = false;
            }
            TxControl::Rollback => {
                self.simple("ROLLBACK").await?;
                self.in_tx = false;
            }
            other => {
                return Err(Error::Unsupported(format!(
                    "transaction control {other:?} (send the statement instead)"
                )));
            }
        }
        Ok(())
    }

    async fn set_auto_commit(&mut self, on: bool) -> Result<()> {
        self.drop_pending().await;
        self.simple(if on {
            "SET autocommit=1"
        } else {
            "SET autocommit=0"
        })
        .await?;
        self.auto_commit = on;
        if on {
            // Switching on commits an open transaction implicitly.
            self.in_tx = false;
        }
        Ok(())
    }

    async fn set_context(&mut self, ctx: LeaseContext) -> Result<()> {
        self.drop_pending().await;
        // MySQL has no schemas below databases: only the database is switched.
        if let Some(db) = ctx.database.as_deref().filter(|d| !d.is_empty()) {
            self.simple(&format!("USE {}", quote_ident(db))).await?;
            // USE is not transactional but also not a data statement: nothing else to track.
        }
        Ok(())
    }

    async fn probe_tx(&mut self) -> Result<TxStatus> {
        Ok(if self.in_tx {
            TxStatus::Active
        } else {
            TxStatus::Idle
        })
    }

    async fn refresh_context(&mut self, executed_text: &str) -> Result<Option<LeaseContext>> {
        if self.conn.is_none() {
            return Ok(None);
        }
        let switched = split_statements(executed_text, self.backslash_escapes)
            .iter()
            .any(|p| starts_with_use(&p.text));
        if !switched {
            return Ok(None);
        }
        Ok(self.query_database().await.map(|db| LeaseContext {
            database: Some(db),
            schema: None,
        }))
    }

    async fn ping(&mut self) -> Result<()> {
        let conn = self.conn()?;
        conn.ping().await.map_err(|e| {
            let mapped = map_query_error(e);
            Error::Connection(mapped.to_string())
        })
    }

    async fn close(&mut self) -> Result<bool> {
        self.drop_pending().await;
        let rolled_back = self.in_tx;
        if let Some(mut conn) = self.conn.take() {
            if self.in_tx {
                let _ = tokio::time::timeout(CLOSE_TIMEOUT, conn.query_drop("ROLLBACK")).await;
                self.in_tx = false;
            }
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, conn.disconnect()).await;
        }
        Ok(rolled_back)
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(MyCanceller {
            killer: self.killer.clone(),
            current: self.current.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(script: &str) -> Vec<String> {
        split_statements(script, true)
            .into_iter()
            .map(|p| p.text)
            .collect()
    }

    #[test]
    fn splits_at_top_level_semicolons() {
        assert_eq!(
            texts("select 1; select ';' ; -- c;\n select `a;b`; /* x; */ ;  "),
            ["select 1", "select ';'", "-- c;\n select `a;b`"]
        );
        assert_eq!(texts("select 1"), ["select 1"]);
        assert!(texts("  # nothing\n /* at all */ ; -- x").is_empty());
        assert_eq!(
            texts("select 'it''s;'; select \"a\\\";\""),
            ["select 'it''s;'", "select \"a\\\";\""]
        );
    }

    #[test]
    fn backslash_escapes_follow_the_sql_mode() {
        assert_eq!(texts("select 'a\\'; select 2").len(), 1);
        let s = split_statements("select 'a\\'; select 2", false);
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].text, "select 2");
    }

    #[test]
    fn ranges_are_character_offsets_without_surrounding_whitespace() {
        let s = split_statements("select 1;\nselect 'é😀'; select 3", true);
        let r: Vec<_> = s.iter().map(|p| p.range).collect();
        assert_eq!(r, [(0, 8), (10, 21), (23, 31)]);
    }

    #[test]
    fn executable_comments_stay_part_of_the_statement() {
        assert_eq!(
            texts("/*!40101 SET x=1 */; select 2"),
            ["/*!40101 SET x=1 */", "select 2"]
        );
        assert_eq!(
            texts("select /*+ MAX_EXECUTION_TIME(5) */ 1; "),
            ["select /*+ MAX_EXECUTION_TIME(5) */ 1"]
        );
        // `--` needs a space, otherwise it is arithmetic.
        assert_eq!(texts("select 1--1; select 2"), ["select 1--1", "select 2"]);
    }

    #[test]
    fn transaction_effects() {
        assert_eq!(tx_effect("START TRANSACTION READ ONLY"), TxEffect::Begin);
        assert_eq!(tx_effect("  begin"), TxEffect::Begin);
        assert_eq!(tx_effect("/* c */ COMMIT"), TxEffect::End);
        assert_eq!(tx_effect("rollback"), TxEffect::End);
        assert_eq!(tx_effect("rollback to savepoint a"), TxEffect::None);
        assert_eq!(tx_effect("CREATE TABLE t (a int)"), TxEffect::End);
        assert_eq!(tx_effect("truncate t"), TxEffect::End);
        assert_eq!(tx_effect("update t set a=1"), TxEffect::Data);
        assert_eq!(tx_effect("select 1"), TxEffect::Data);
        assert_eq!(tx_effect("set autocommit=0"), TxEffect::None);
        assert_eq!(tx_effect("use db"), TxEffect::None);
        assert!(starts_with_use("  USE `db`"));
        assert!(!starts_with_use("select 'use'"));
    }
}
