//! Statement execution: scripts, table pages and streaming through server-side cursors.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use dbstudio_core::{ColumnMeta, EndOutcome, Error, Result, ResultSink, StatementEnd, Value};
use tokio_postgres::types::Type;
use tokio_postgres::{SimpleQueryMessage, SimpleQueryRow};

use crate::conn::PooledConn;
use crate::decode::{decode_text, type_label, value_kind};
use crate::error::{map_error, map_statement_error};
use crate::sql::{self, Stmt};

/// Default rows per streamed batch.
pub const DEFAULT_BATCH: usize = 1000;

/// Cancellation state of a running request.
#[derive(Debug, Default)]
pub struct Running {
    pub cancelled: AtomicBool,
}

impl Running {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Column metadata and types of a prepared statement.
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

fn flush_notices(conn: &mut PooledConn, sink: &mut dyn ResultSink) -> Result<()> {
    for message in conn.drain_notices() {
        sink.on_message(message)?;
    }
    Ok(())
}

/// Run every statement of `text`, returning how many were executed.
pub async fn run_script(
    conn: &mut PooledConn,
    text: &str,
    batch: usize,
    max_rows: Option<u64>,
    sink: &mut dyn ResultSink,
    running: &Running,
) -> Result<u32> {
    let mut count = 0;
    for stmt in sql::split_statements(text) {
        if running.is_cancelled() {
            return Err(Error::Cancelled);
        }
        run_statement(conn, &stmt, batch, max_rows, sink, running).await?;
        count += 1;
    }
    Ok(count)
}

async fn run_statement(
    conn: &mut PooledConn,
    stmt: &Stmt,
    batch: usize,
    max_rows: Option<u64>,
    sink: &mut dyn ResultSink,
    running: &Running,
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
            conn, &stmt.text, &shape, batch, max_rows, false, sink, running, started,
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
    if let Some(max) = max_rows {
        if rows.len() as u64 > max {
            rows.truncate(max as usize);
            truncated = true;
        }
    }
    let total = rows.len() as u64;
    flush_notices(conn, sink)?;
    while !rows.is_empty() {
        let rest = rows.split_off(batch.min(rows.len()));
        sink.on_rows(std::mem::replace(&mut rows, rest))?;
    }
    sink.on_statement_end(StatementEnd {
        outcome: EndOutcome::Rows {
            row_count: total,
            truncated,
        },
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
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
/// one; `read_only` makes that transaction `READ ONLY`.
#[allow(clippy::too_many_arguments)]
async fn stream_cursor(
    conn: &mut PooledConn,
    select: &str,
    shape: &Shape,
    batch: usize,
    max_rows: Option<u64>,
    read_only: bool,
    sink: &mut dyn ResultSink,
    running: &Running,
    started: Instant,
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

    let result = fetch_loop(conn, &name, shape, batch, max_rows, sink, running, started).await;

    // Cleanup: commit our own transaction on success (statements may have side effects),
    // otherwise roll it back / close the cursor of the user's transaction.
    match (&result, own_txn) {
        (Ok(()), true) => {
            let committed = conn.client.batch_execute("COMMIT").await;
            conn.in_txn = false;
            if let Err(e) = committed {
                return Err(CursorError::Other(map_error(e)));
            }
        }
        (Err(_), true) => {
            let _ = conn.client.batch_execute("ROLLBACK").await;
            conn.in_txn = false;
        }
        (_, false) => {
            let _ = conn.client.batch_execute(&format!("CLOSE {name}")).await;
        }
    }
    result.map_err(CursorError::Other)
}

#[allow(clippy::too_many_arguments)]
async fn fetch_loop(
    conn: &mut PooledConn,
    cursor: &str,
    shape: &Shape,
    batch: usize,
    max_rows: Option<u64>,
    sink: &mut dyn ResultSink,
    running: &Running,
    started: Instant,
) -> Result<()> {
    sink.on_columns(shape.metas.clone())?;
    let mut total: u64 = 0;
    let mut truncated = false;
    loop {
        // One extra row tells us whether `max_rows` cut the result short.
        let want = match max_rows {
            Some(max) => (batch as u64).min(max - total + 1) as usize,
            None => batch,
        };
        let messages = conn
            .client
            .simple_query(&format!("FETCH FORWARD {want} FROM {cursor}"))
            .await
            .map_err(|e| map_statement_error(e, 0, running.is_cancelled()))?;
        let mut rows: Vec<Vec<Value>> = messages
            .iter()
            .filter_map(|m| match m {
                SimpleQueryMessage::Row(r) => Some(decode_row(r, &shape.types)),
                _ => None,
            })
            .collect();
        let got = rows.len();
        if let Some(max) = max_rows {
            if total + got as u64 > max {
                rows.truncate((max - total) as usize);
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
        if truncated || got < want {
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
    })
}

/// Stream a generated table `SELECT` (always read-only) into `sink`.
pub async fn run_table_select(
    conn: &mut PooledConn,
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
        conn, select, &shape, batch, None, true, sink, running, started,
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
pub async fn run_count(conn: &mut PooledConn, select: &str) -> Result<u64> {
    conn.client
        .batch_execute("BEGIN READ ONLY")
        .await
        .map_err(map_error)?;
    conn.in_txn = true;
    let result = conn.client.query_one(select, &[]).await;
    let _ = conn.client.batch_execute("ROLLBACK").await;
    conn.in_txn = false;
    let row = result.map_err(map_error)?;
    let n: i64 = row.get(0);
    Ok(n.max(0) as u64)
}
