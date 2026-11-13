//! Statement execution: scripts, table pages and streaming through server-side cursors.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use dbstudio_core::{
    ColumnMeta, EndOutcome, Error, Result, ResultSink, ScriptDialect, StatementEnd, StatementStart,
    TextRange, Value,
};
use tokio_postgres::types::Type;
use tokio_postgres::{SimpleQueryMessage, SimpleQueryRow};

use crate::conn::Conn;
use crate::decode::{decode_text, type_label, value_kind};
use crate::error::{map_error, map_statement_error};
use crate::sql::{self, Stmt};

/// Default rows per streamed batch.
pub const DEFAULT_BATCH: usize = 1000;

/// Cancellation state of a running request.
///
/// `gate` serialises "send a cancel request" against "the request ends": a cancel is only sent
/// while the request has not retired, and a request that was (or is being) cancelled never
/// returns its connection to the pool, so a late cancel can never hit a later statement.
#[derive(Debug, Default)]
pub struct Running {
    pub cancelled: AtomicBool,
    gate: tokio::sync::Mutex<bool>,
}

impl Running {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Mark the request finished (waiting for an in-flight cancel send); returns whether a
    /// cancel was requested, in which case the connection must be discarded.
    pub async fn retire(&self) -> bool {
        *self.gate.lock().await = true;
        self.is_cancelled()
    }

    /// Flag the request as cancelled and, if it is still running, send the cancel request.
    pub async fn send_cancel(
        &self,
        shared: &crate::conn::Shared,
        token: &tokio_postgres::CancelToken,
    ) -> Result<()> {
        let finished = self.gate.lock().await;
        if *finished {
            return Ok(());
        }
        self.cancelled.store(true, Ordering::Release);
        shared.cancel_token(token).await
    }
}

/// Column metadata and types of a prepared statement.
#[derive(Clone)]
struct Shape {
    metas: Vec<ColumnMeta>,
    types: Vec<Type>,
}

fn shape_of(stmt: &tokio_postgres::Statement) -> Shape {
    Shape {
        metas: stmt
            .columns()
            .iter()
            .map(|c| ColumnMeta {
                name: c.name().to_string(),
                type_name: type_label(c.type_()),
                value_kind: value_kind(c.type_()),
                nullable: None,
            })
            .collect(),
        types: stmt.columns().iter().map(|c| c.type_().clone()).collect(),
    }
}

fn decode_row(row: &SimpleQueryRow, types: &[Type]) -> Vec<Value> {
    types
        .iter()
        .enumerate()
        .map(|(i, ty)| match row.get(i) {
            Some(raw) => decode_text(ty, raw),
            None => Value::Null,
        })
        .collect()
}

fn flush_notices(conn: &mut Conn, sink: &mut dyn ResultSink) -> Result<()> {
    for message in conn.drain_notices() {
        sink.on_message(message)?;
    }
    Ok(())
}

/// Result left open on a lease after the last statement hit `max_rows` (`fetch_more`).
///
/// Either a server-side cursor (rows are fetched on demand) or, for statements a cursor cannot
/// host, the rows that did not fit, kept in memory.
pub struct PendingCursor {
    /// Cursor name; `None` when the remaining rows are buffered in `lookahead`.
    name: Option<String>,
    shape: Shape,
    /// The cursor lives in a transaction opened just for it (auto-commit mode).
    own_txn: bool,
    /// Rows already read from the server (or buffered) but not delivered yet.
    lookahead: Vec<Vec<Value>>,
}

impl PendingCursor {
    /// True when the cursor holds a transaction of its own (not the user's).
    pub fn own_txn(&self) -> bool {
        self.own_txn
    }
}

/// How [`run_script_ex`] runs a script.
#[derive(Debug, Clone, Copy)]
pub struct ScriptOpts {
    /// Rows per delivered batch.
    pub batch: usize,
    /// Rows per result set.
    pub max_rows: Option<u64>,
    /// Open a transaction before the first statement that is not transaction control (manual
    /// commit mode).
    pub lazy_begin: bool,
    /// Leave the last statement's result open when it hits `max_rows`.
    pub keep_open: bool,
}

/// Run every statement of `text`, returning how many were executed.
pub async fn run_script(
    conn: &mut Conn,
    text: &str,
    batch: usize,
    max_rows: Option<u64>,
    sink: &mut dyn ResultSink,
    running: &Running,
) -> Result<u32> {
    let opts = ScriptOpts {
        batch,
        max_rows,
        lazy_begin: false,
        keep_open: false,
    };
    run_script_ex(conn, text, &opts, sink, running, &mut None).await
}

/// Statements of `text` as found by the shared splitter (`dbstudio-sql`).
fn split_script(text: &str) -> Vec<Stmt> {
    let ranges = dbstudio_sql::split(ScriptDialect::Postgres, text);
    if ranges.is_empty() {
        return Vec::new();
    }
    // Byte offset of every character (plus the end) to slice by character ranges.
    let mut offsets: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    offsets.push(text.len());
    ranges
        .into_iter()
        .filter_map(|r| {
            let from = *offsets.get(r.start as usize)?;
            let to = *offsets.get(r.end as usize)?;
            Some(Stmt {
                text: text[from..to].to_string(),
                char_offset: r.start as usize,
            })
        })
        .collect()
}

/// First keyword of a statement is transaction control (no implicit `BEGIN` before it).
fn is_tx_control(stmt: &str) -> bool {
    matches!(
        sql::words(stmt).first().map(String::as_str),
        Some(
            "BEGIN"
                | "START"
                | "COMMIT"
                | "END"
                | "ROLLBACK"
                | "ABORT"
                | "SAVEPOINT"
                | "RELEASE"
                | "PREPARE"
        )
    )
}

/// Statements PostgreSQL refuses inside a transaction block: manual commit mode must not open
/// its implicit `BEGIN` before them.
fn runs_outside_tx(stmt: &str) -> bool {
    let w = sql::words(stmt);
    let first = w.first().map(String::as_str);
    let second = w.get(1).map(String::as_str);
    match (first, second) {
        (Some("VACUUM"), _) => true,
        (Some("ALTER"), Some("SYSTEM")) => true,
        (Some("CREATE" | "DROP"), Some("DATABASE" | "TABLESPACE")) => true,
        (Some("CREATE" | "DROP" | "REINDEX"), _) => w.iter().any(|x| x == "CONCURRENTLY"),
        _ => false,
    }
}

/// Whether the open transaction block is aborted: any statement but `ROLLBACK` is refused with
/// `25P02` in that state. Used when the cause of a failure does not say it (a cancelled or
/// stopped request that may or may not have reached the server).
pub async fn probe_aborted(conn: &Conn) -> bool {
    let probe = conn.client.simple_query("SELECT 1");
    match tokio::time::timeout(std::time::Duration::from_secs(2), probe).await {
        Ok(Err(e)) => e.as_db_error().is_some_and(|d| d.code().code() == "25P02"),
        _ => false,
    }
}

/// Like [`run_script`] with lease features: statement start events, lazy `BEGIN`, tracking of
/// aborted transactions (`conn.failed`) and a result that stays open in `pending`.
pub async fn run_script_ex(
    conn: &mut Conn,
    text: &str,
    opts: &ScriptOpts,
    sink: &mut dyn ResultSink,
    running: &Running,
    pending: &mut Option<PendingCursor>,
) -> Result<u32> {
    let stmts = split_script(text);
    let last = stmts.len().saturating_sub(1);
    let mut count = 0;
    for (index, stmt) in stmts.iter().enumerate() {
        if running.is_cancelled() {
            return Err(Error::Cancelled);
        }
        // The splitter keeps leading whitespace in the statement text; the range does not.
        let lead = stmt.text.chars().take_while(|c| c.is_whitespace()).count();
        let start = (stmt.char_offset + lead) as u32;
        sink.on_statement_start(StatementStart {
            index: index as u32,
            range: TextRange {
                start,
                end: start + stmt.text.trim().chars().count() as u32,
            },
        })?;
        if opts.lazy_begin
            && !conn.in_txn
            && !is_tx_control(&stmt.text)
            && !runs_outside_tx(&stmt.text)
        {
            conn.client
                .batch_execute("BEGIN")
                .await
                .map_err(|e| map_statement_error(e, stmt.char_offset, running.is_cancelled()))?;
            conn.in_txn = true;
            conn.failed = false;
        }
        let keep = opts.keep_open && index == last;
        let result = run_statement(
            conn,
            stmt,
            opts.batch,
            opts.max_rows,
            sink,
            running,
            keep,
            pending,
        )
        .await;
        match result {
            Ok(()) => {
                // A statement that succeeded proves the block is healthy again (`ROLLBACK TO`).
                conn.failed = false;
            }
            Err(e) => {
                if conn.in_txn {
                    match e {
                        // The server refused the statement: the block is aborted.
                        Error::Query { .. } => conn.failed = true,
                        // Stopped by the client or the sink: only a server-side cancel aborts.
                        Error::Cancelled => conn.failed = probe_aborted(conn).await,
                        _ => {}
                    }
                }
                return Err(e);
            }
        }
        count += 1;
    }
    Ok(count)
}

#[allow(clippy::too_many_arguments)]
async fn run_statement(
    conn: &mut Conn,
    stmt: &Stmt,
    batch: usize,
    max_rows: Option<u64>,
    sink: &mut dyn ResultSink,
    running: &Running,
    keep_open: bool,
    pending: &mut Option<PendingCursor>,
) -> Result<()> {
    let started = Instant::now();
    let err = |e| map_statement_error(e, stmt.char_offset, running.is_cancelled());
    let prepared = conn.client.prepare(&stmt.text).await.map_err(err)?;
    let shape = shape_of(&prepared);
    drop(prepared);

    if shape.metas.is_empty() {
        let messages = conn.client.simple_query(&stmt.text).await.map_err(err)?;
        let count = messages
            .iter()
            .rev()
            .find_map(|m| match m {
                SimpleQueryMessage::CommandComplete(n) => Some(*n),
                _ => None,
            })
            .unwrap_or(0);
        conn.in_txn = sql::txn_effect(&stmt.text, conn.in_txn);
        flush_notices(conn, sink)?;
        let outcome = if sql::is_dml(&stmt.text) || count > 0 {
            EndOutcome::Affected { rows: count }
        } else {
            EndOutcome::Done
        };
        return sink.on_statement_end(StatementEnd {
            outcome,
            elapsed_ms: started.elapsed().as_millis() as u64,
        });
    }

    if sql::cursor_eligible(&stmt.text) {
        match stream_cursor(
            conn, &stmt.text, &shape, batch, max_rows, false, sink, running, started, keep_open,
            pending,
        )
        .await
        {
            Err(CursorError::DeclareRejected) => {}
            Err(CursorError::Other(e)) => return Err(e),
            Ok(()) => return Ok(()),
        }
    }

    // Statements a cursor cannot host (EXPLAIN, SHOW, ... RETURNING): buffered execution.
    let messages = conn.client.simple_query(&stmt.text).await.map_err(err)?;
    conn.in_txn = sql::txn_effect(&stmt.text, conn.in_txn);
    sink.on_columns(shape.metas.clone())?;
    let mut rows: Vec<Vec<Value>> = messages
        .iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some(decode_row(r, &shape.types)),
            _ => None,
        })
        .collect();
    let mut truncated = false;
    let mut rest: Vec<Vec<Value>> = Vec::new();
    if let Some(max) = max_rows {
        if rows.len() as u64 > max {
            rest = rows.split_off(max as usize);
            truncated = true;
        }
    }
    let total = rows.len() as u64;
    flush_notices(conn, sink)?;
    while !rows.is_empty() {
        let tail = rows.split_off(batch.min(rows.len()));
        sink.on_rows(std::mem::replace(&mut rows, tail))?;
    }
    sink.on_statement_end(StatementEnd {
        outcome: EndOutcome::Rows {
            row_count: total,
            truncated,
        },
        elapsed_ms: started.elapsed().as_millis() as u64,
    })?;
    if keep_open && truncated {
        *pending = Some(PendingCursor {
            name: None,
            shape,
            own_txn: false,
            lookahead: rest,
        });
    }
    Ok(())
}

/// Failure of [`stream_cursor`].
enum CursorError {
    /// `DECLARE CURSOR` refused the statement before anything was sent to the sink.
    DeclareRejected,
    Other(Error),
}

impl From<Error> for CursorError {
    fn from(e: Error) -> Self {
        CursorError::Other(e)
    }
}

/// Stream the rows of a `SELECT`-like statement through a server-side cursor, `batch` rows per
/// round trip, so memory stays bounded and a stopping sink never leaves a half-read result on
/// the connection. Statements run in their own transaction unless the script already opened
/// one; `read_only` makes that transaction `READ ONLY`. With `keep_open`, a result cut short by
/// `max_rows` leaves the cursor (and its transaction) open in `pending`.
#[allow(clippy::too_many_arguments)]
async fn stream_cursor(
    conn: &mut Conn,
    select: &str,
    shape: &Shape,
    batch: usize,
    max_rows: Option<u64>,
    read_only: bool,
    sink: &mut dyn ResultSink,
    running: &Running,
    started: Instant,
    keep_open: bool,
    pending: &mut Option<PendingCursor>,
) -> std::result::Result<(), CursorError> {
    let cancelled = || running.is_cancelled();
    let own_txn = !conn.in_txn;
    if own_txn {
        let begin = if read_only {
            "BEGIN READ ONLY"
        } else {
            "BEGIN"
        };
        conn.client
            .batch_execute(begin)
            .await
            .map_err(|e| map_statement_error(e, 0, cancelled()))?;
        conn.in_txn = true;
    }
    let name = conn.next_cursor_name();
    let declare = format!("DECLARE {name} NO SCROLL CURSOR FOR\n{select}");
    if let Err(e) = conn.client.batch_execute(&declare).await {
        if own_txn {
            let _ = conn.client.batch_execute("ROLLBACK").await;
            conn.in_txn = false;
        }
        let rejected = e
            .as_db_error()
            .is_some_and(|d| d.code().code() == "0A000" || d.code().code() == "42601");
        return Err(if rejected && own_txn {
            CursorError::DeclareRejected
        } else {
            CursorError::Other(map_statement_error(e, 0, cancelled()))
        });
    }

    let mut lookahead = Vec::new();
    let result = fetch_loop(
        conn,
        Some(&name),
        shape,
        batch,
        max_rows,
        sink,
        running,
        started,
        &mut lookahead,
        true,
    )
    .await;

    if let (Ok(true), true) = (&result, keep_open) {
        *pending = Some(PendingCursor {
            name: Some(name),
            shape: shape.clone(),
            own_txn,
            lookahead,
        });
        return Ok(());
    }
    // Cleanup: commit our own transaction on success (statements may have side effects),
    // otherwise roll it back / close the cursor of the user's transaction.
    match close_cursor(conn, Some(&name), own_txn, result.is_ok()).await {
        Ok(()) => result.map(|_| ()).map_err(CursorError::Other),
        Err(e) if result.is_ok() => Err(CursorError::Other(e)),
        Err(_) => result.map(|_| ()).map_err(CursorError::Other),
    }
}

/// Release a cursor: commit (`ok`) or roll back its own transaction, or close it inside the
/// user's transaction.
async fn close_cursor(conn: &mut Conn, name: Option<&str>, own_txn: bool, ok: bool) -> Result<()> {
    match (own_txn, ok) {
        (true, true) => {
            let committed = conn.client.batch_execute("COMMIT").await;
            conn.in_txn = false;
            committed.map_err(map_error)
        }
        (true, false) => {
            let _ = conn.client.batch_execute("ROLLBACK").await;
            conn.in_txn = false;
            Ok(())
        }
        (false, _) => {
            if let Some(name) = name {
                let _ = conn.client.batch_execute(&format!("CLOSE {name}")).await;
            }
            Ok(())
        }
    }
}

/// Drop a pending result (releases the cursor and the transaction held for it).
pub async fn discard_pending(conn: &mut Conn, pending: PendingCursor) -> Result<()> {
    if conn.client.is_closed() {
        conn.in_txn = false;
        return Ok(());
    }
    close_cursor(conn, pending.name.as_deref(), pending.own_txn, true).await
}

/// Deliver the next `max_rows` rows of a pending result. Returns the pending result again while
/// more rows remain; otherwise the cursor is already released.
pub async fn fetch_pending(
    conn: &mut Conn,
    mut pending: PendingCursor,
    max_rows: Option<u64>,
    batch: usize,
    sink: &mut dyn ResultSink,
    running: &Running,
) -> Result<Option<PendingCursor>> {
    let started = Instant::now();
    let mut lookahead = std::mem::take(&mut pending.lookahead);
    let result = fetch_loop(
        conn,
        pending.name.as_deref(),
        &pending.shape,
        batch,
        max_rows,
        sink,
        running,
        started,
        &mut lookahead,
        false,
    )
    .await;
    match result {
        Ok(true) => {
            pending.lookahead = lookahead;
            Ok(Some(pending))
        }
        Ok(false) => {
            close_cursor(conn, pending.name.as_deref(), pending.own_txn, true).await?;
            Ok(None)
        }
        Err(e) => {
            let _ = close_cursor(conn, pending.name.as_deref(), pending.own_txn, false).await;
            Err(e)
        }
    }
}

/// Deliver rows of a result: first the buffered `lookahead`, then batches fetched from `cursor`
/// (when there is one). Returns whether more rows remain; the row after `max_rows` that proves
/// it is kept in `lookahead` for the next call.
#[allow(clippy::too_many_arguments)]
async fn fetch_loop(
    conn: &mut Conn,
    cursor: Option<&str>,
    shape: &Shape,
    batch: usize,
    max_rows: Option<u64>,
    sink: &mut dyn ResultSink,
    running: &Running,
    started: Instant,
    lookahead: &mut Vec<Vec<Value>>,
    emit_columns: bool,
) -> Result<bool> {
    if emit_columns {
        sink.on_columns(shape.metas.clone())?;
    }
    let mut total: u64 = 0;
    let mut truncated = false;
    loop {
        // One extra row tells us whether `max_rows` cut the result short.
        let want = match max_rows {
            Some(max) => (batch as u64).min(max.saturating_sub(total) + 1) as usize,
            None => batch,
        };
        let from_buffer = !lookahead.is_empty();
        let mut rows: Vec<Vec<Value>> = if from_buffer {
            std::mem::take(lookahead)
        } else if let Some(cursor) = cursor {
            let messages = conn
                .client
                .simple_query(&format!("FETCH FORWARD {want} FROM {cursor}"))
                .await
                .map_err(|e| map_statement_error(e, 0, running.is_cancelled()))?;
            messages
                .iter()
                .filter_map(|m| match m {
                    SimpleQueryMessage::Row(r) => Some(decode_row(r, &shape.types)),
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        let got = rows.len();
        if let Some(max) = max_rows {
            if total + got as u64 > max {
                *lookahead = rows.split_off(max.saturating_sub(total) as usize);
                truncated = true;
            }
        }
        total += rows.len() as u64;
        flush_notices(conn, sink)?;
        if !rows.is_empty() {
            sink.on_rows(rows)?;
        }
        if running.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if truncated || (!from_buffer && got < want) {
            break;
        }
    }
    flush_notices(conn, sink)?;
    sink.on_statement_end(StatementEnd {
        outcome: EndOutcome::Rows {
            row_count: total,
            truncated,
        },
        elapsed_ms: started.elapsed().as_millis() as u64,
    })?;
    Ok(truncated)
}

/// Stream a generated table `SELECT` (always read-only) into `sink`.
pub async fn run_table_select(
    conn: &mut Conn,
    select: &str,
    batch: usize,
    sink: &mut dyn ResultSink,
    running: &Running,
) -> Result<()> {
    let started = Instant::now();
    let prepared = conn
        .client
        .prepare(select)
        .await
        .map_err(|e| map_statement_error(e, 0, running.is_cancelled()))?;
    let shape = shape_of(&prepared);
    drop(prepared);
    match stream_cursor(
        conn, select, &shape, batch, None, true, sink, running, started, false, &mut None,
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(CursorError::Other(e)) => Err(e),
        Err(CursorError::DeclareRejected) => Err(Error::query(
            "the table query cannot be streamed with a cursor".to_string(),
        )),
    }
}

/// Run a `SELECT count(*)` in a read-only transaction.
pub async fn run_count(conn: &mut Conn, select: &str, running: &Running) -> Result<u64> {
    let cancelled = || running.is_cancelled();
    conn.client
        .batch_execute("BEGIN READ ONLY")
        .await
        .map_err(|e| map_statement_error(e, 0, cancelled()))?;
    conn.in_txn = true;
    let result = conn.client.query_one(select, &[]).await;
    let _ = conn.client.batch_execute("ROLLBACK").await;
    conn.in_txn = false;
    let row = result.map_err(|e| map_statement_error(e, 0, cancelled()))?;
    let n: i64 = row.get(0);
    Ok(n.max(0) as u64)
}
