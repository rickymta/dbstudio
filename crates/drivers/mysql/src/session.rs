//! The [`Session`] implementation: pool management, query streaming and cancellation.
//!
//! # Streaming
//!
//! Statements run through the text protocol with `query_iter`, which yields rows one at a time
//! as they are read from the socket; rows are decoded and pushed to the sink in batches, so
//! memory use is bounded by the batch size no matter how large the result is.
//!
//! # Cancellation
//!
//! Every running statement registers a [`Ctl`] under its `query_id`. [`Session::cancel`] (or the
//! `timeout_ms` timer) opens a *separate* connection and sends `KILL QUERY <connection id>`.
//! A sink error takes the same path internally. After a kill the remaining protocol data of the
//! killed statement is drained so the connection can safely return to the pool; if draining
//! fails or takes too long the connection is closed instead. A per-statement mutex guarantees a
//! late `KILL` can never hit a later statement of the same pooled connection.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dbstudio_core::{
    ColumnMeta, EndOutcome, Error, ExecSummary, ObjectNode, ObjectPath, QueryRequest, Result,
    ResultSink, ServerInfo, Session, StatementEnd, TableCountRequest, TableDataRequest,
    TableDetails, Value,
};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, Pool, Row};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::error::{map_connect_error, map_query_error};
use crate::sql::{check_raw_where, count_sql, quote_ident, select_sql, table_target};
use crate::value::{ColInfo, decode_cell};
use crate::{browse, detect_mariadb, normalize_version};

const DEFAULT_BATCH: usize = 1000;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a statement is being stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KillReason {
    /// User requested cancellation.
    Cancel,
    /// `timeout_ms` elapsed.
    Timeout,
    /// Internal abort (the sink asked to stop).
    Abort,
}

#[derive(Debug, Default)]
struct CtlState {
    finished: bool,
    killed: bool,
}

/// Control block of one running statement.
#[derive(Debug)]
pub(crate) struct Ctl {
    conn_id: u32,
    cancelled: AtomicBool,
    timed_out: AtomicBool,
    state: tokio::sync::Mutex<CtlState>,
}

impl Ctl {
    fn new(conn_id: u32) -> Self {
        Ctl {
            conn_id,
            cancelled: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            state: tokio::sync::Mutex::new(CtlState::default()),
        }
    }

    pub(crate) fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst) || self.timed_out.load(Ordering::SeqCst)
    }

    pub(crate) async fn kill(&self, inner: &Inner, reason: KillReason) {
        let mut st = self.state.lock().await;
        if st.finished || st.killed {
            return;
        }
        match reason {
            KillReason::Cancel => self.cancelled.store(true, Ordering::SeqCst),
            KillReason::Timeout => self.timed_out.store(true, Ordering::SeqCst),
            KillReason::Abort => {}
        }
        st.killed = true;
        inner.kill_query(self.conn_id).await;
    }

    async fn finish(&self) {
        self.state.lock().await.finished = true;
    }

    /// Turn a statement result into the final one, honouring cancel / timeout flags.
    fn finalize<T>(&self, res: Result<T>) -> Result<T> {
        if self.timed_out.load(Ordering::SeqCst) {
            Err(Error::Timeout)
        } else if self.cancelled.load(Ordering::SeqCst) {
            Err(Error::Cancelled)
        } else {
            res
        }
    }
}

/// Key columns per `(database, table)` with the time they were read.
type KeyCache = HashMap<(String, String), (Instant, Arc<Vec<String>>)>;

/// Shared state of a session.
pub(crate) struct Inner {
    pub(crate) pool: Pool,
    pub(crate) opts: Opts,
    running: Mutex<HashMap<Uuid, Arc<Ctl>>>,
    closed: AtomicBool,
    acquire_timeout: Duration,
    secret: Option<String>,
    pub(crate) is_mariadb: bool,
    /// Default database of the configuration (restored after a per-request override).
    default_db: Option<String>,
    /// Ids of pooled connections whose current database was changed by a request. The pool's
    /// `COM_RESET_CONNECTION` does not undo `USE`, so these are repaired lazily.
    dirty_db: Mutex<HashSet<u32>>,
    /// Key columns per `(database, table)` with the time they were read.
    keys: Mutex<KeyCache>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("is_mariadb", &self.is_mariadb)
            .finish_non_exhaustive()
    }
}

/// Guard of a registered statement; see [`Inner::begin`].
struct QueryGuard {
    inner: Arc<Inner>,
    id: Uuid,
    ctl: Arc<Ctl>,
    timer: Option<JoinHandle<()>>,
}

impl QueryGuard {
    /// Mark the statement finished (waiting for any in-flight `KILL`), unregister it.
    async fn end(self) -> Arc<Ctl> {
        self.ctl.finish().await;
        if let Some(t) = &self.timer {
            t.abort();
        }
        self.inner
            .running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
        self.ctl.clone()
    }
}

impl Inner {
    pub(crate) fn new(
        pool: Pool,
        opts: Opts,
        acquire_timeout: Duration,
        secret: Option<String>,
        is_mariadb: bool,
        default_db: Option<String>,
    ) -> Self {
        Inner {
            pool,
            opts,
            running: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            acquire_timeout,
            secret,
            is_mariadb,
            default_db,
            dirty_db: Mutex::new(HashSet::new()),
            keys: Mutex::new(HashMap::new()),
        }
    }

    /// Columns that make row order unique: the primary key, else the first unique index made of
    /// NOT NULL, non-prefix columns; empty for views and keyless tables. Cached for a minute.
    pub(crate) async fn key_columns(
        &self,
        conn: &mut Conn,
        db: &str,
        table: &str,
    ) -> Result<Arc<Vec<String>>> {
        const TTL: Duration = Duration::from_secs(60);
        let key = (db.to_string(), table.to_string());
        if let Some((at, cols)) = self
            .keys
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
        {
            if at.elapsed() < TTL {
                return Ok(cols.clone());
            }
        }
        let rows: Vec<Row> = conn
            .exec(
                "SELECT INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME, NULLABLE, SUB_PART \
                 FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                 ORDER BY INDEX_NAME = 'PRIMARY' DESC, NON_UNIQUE, INDEX_NAME, SEQ_IN_INDEX",
                (db, table),
            )
            .await
            .map_err(map_query_error)?;
        // Group consecutive rows by index; pick PRIMARY, else the first fully usable unique one.
        let mut chosen: Vec<String> = Vec::new();
        let mut current: Option<(String, bool, Vec<String>)> = None;
        let finish = |cur: Option<(String, bool, Vec<String>)>, chosen: &mut Vec<String>| {
            if let Some((_, usable, cols)) = cur {
                if chosen.is_empty() && usable && !cols.is_empty() {
                    *chosen = cols;
                }
            }
        };
        for r in &rows {
            let name = cell(r, 0).unwrap_or_default();
            let unique = cell(r, 1).as_deref() == Some("0");
            let col = cell(r, 3);
            let usable_col =
                col.is_some() && cell(r, 4).unwrap_or_default().is_empty() && cell(r, 5).is_none();
            if current.as_ref().is_none_or(|c| c.0 != name) {
                finish(current.take(), &mut chosen);
                current = Some((name.clone(), unique, Vec::new()));
            }
            if let Some(c) = current.as_mut() {
                c.1 = c.1 && usable_col;
                if let Some(col) = col {
                    c.2.push(col);
                }
            }
        }
        finish(current.take(), &mut chosen);
        let cols = Arc::new(chosen);
        self.keys
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key, (Instant::now(), cols.clone()));
        Ok(cols)
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            Err(Error::Connection("the session is closed".into()))
        } else {
            Ok(())
        }
    }

    /// Take a connection from the pool.
    pub(crate) async fn acquire(&self) -> Result<Conn> {
        self.ensure_open()?;
        match tokio::time::timeout(self.acquire_timeout, self.pool.get_conn()).await {
            Err(_) => Err(Error::Timeout),
            Ok(Err(e)) => Err(map_connect_error(e, self.secret.as_deref())),
            Ok(Ok(c)) => Ok(c),
        }
    }

    /// Acquire a connection positioned in `database` (when given) or in the configured default
    /// database (repairing a connection left in another database by an earlier request).
    async fn acquire_in(&self, database: Option<&str>) -> Result<Conn> {
        let mut attempts = 0;
        loop {
            let mut conn = self.acquire().await?;
            let id = conn.id();
            if let Some(db) = database {
                conn.query_drop(format!("USE {}", quote_ident(db)))
                    .await
                    .map_err(map_query_error)?;
                self.dirty_db
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(id);
                return Ok(conn);
            }
            let dirty = self
                .dirty_db
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            if !dirty {
                return Ok(conn);
            }
            match &self.default_db {
                Some(d) => {
                    conn.query_drop(format!("USE {}", quote_ident(d)))
                        .await
                        .map_err(map_query_error)?;
                    return Ok(conn);
                }
                None => {
                    // A connection cannot be put back to "no database"; replace it.
                    let _ = conn.disconnect().await;
                    attempts += 1;
                    if attempts > 3 {
                        return self.acquire().await;
                    }
                }
            }
        }
    }

    /// Return a connection; broken ones are closed instead of recycled.
    pub(crate) async fn release(&self, conn: Conn, broken: bool) {
        if broken {
            let _ = conn.disconnect().await;
        }
        // Otherwise dropping hands it back to the pool, which resets the session state.
    }

    /// `KILL QUERY` through a dedicated short-lived connection (never the pool, which may be
    /// exhausted by the very statements we are trying to stop).
    async fn kill_query(&self, conn_id: u32) {
        let opts = self.opts.clone();
        let work = async move {
            let mut c = Conn::new(opts).await?;
            c.query_drop(format!("KILL QUERY {conn_id}")).await?;
            c.disconnect().await
        };
        match tokio::time::timeout(KILL_TIMEOUT, work).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("KILL QUERY {conn_id} failed: {e}"),
            Err(_) => tracing::warn!("KILL QUERY {conn_id} timed out"),
        }
    }

    fn begin(self: &Arc<Self>, conn_id: u32, id: Uuid, timeout_ms: Option<u64>) -> QueryGuard {
        let ctl = Arc::new(Ctl::new(conn_id));
        self.running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, ctl.clone());
        let timer = timeout_ms.map(|ms| {
            let inner = self.clone();
            let ctl = ctl.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                ctl.kill(&inner, KillReason::Timeout).await;
            })
        });
        QueryGuard {
            inner: self.clone(),
            id,
            ctl,
            timer,
        }
    }
}

/// Convert any protocol value to its string form (used for metadata queries).
pub(crate) fn value_to_string(v: &mysql_async::Value) -> Option<String> {
    use mysql_async::Value as V;
    Some(match v {
        V::NULL => return None,
        V::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        V::Int(i) => i.to_string(),
        V::UInt(u) => u.to_string(),
        V::Float(f) => f.to_string(),
        V::Double(d) => d.to_string(),
        V::Date(y, mo, d, h, mi, s, us) => {
            if *h == 0 && *mi == 0 && *s == 0 && *us == 0 {
                format!("{y:04}-{mo:02}-{d:02}")
            } else if *us == 0 {
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

/// String of the cell at `i`, `None` for NULL or a missing column.
pub(crate) fn cell(row: &Row, i: usize) -> Option<String> {
    row.as_ref(i).and_then(value_to_string)
}

/// String of the cell in the column named `name` (case-insensitive).
pub(crate) fn cell_named(row: &Row, name: &str) -> Option<String> {
    let idx = row
        .columns_ref()
        .iter()
        .position(|c| c.name_str().eq_ignore_ascii_case(name))?;
    cell(row, idx)
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
        other => value_to_string(&other)
            .map(Value::Text)
            .unwrap_or(Value::Null),
    }
}

fn decode_row(infos: &[ColInfo], row: Row) -> Vec<Value> {
    row.unwrap()
        .into_iter()
        .enumerate()
        .map(|(i, v)| match infos.get(i) {
            Some(info) => decode_value(info, v),
            None => value_to_string(&v).map(Value::Text).unwrap_or(Value::Null),
        })
        .collect()
}

struct StreamOpts {
    batch: usize,
    max_rows: Option<u64>,
}

fn elapsed_ms(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Run `sql` (possibly several statements) and stream every result into `sink`.
/// Returns the result plus whether the connection must be discarded.
async fn stream_query(
    inner: &Inner,
    conn: &mut Conn,
    ctl: &Ctl,
    sql: &str,
    o: &StreamOpts,
    sink: &mut dyn ResultSink,
    statements: &mut u32,
) -> (Result<()>, bool) {
    let mut result = match conn.query_iter(sql).await {
        Ok(r) => r,
        Err(e) => return (Err(map_query_error(e)), false),
    };
    let mut failure: Option<Error> = None;
    // True when `failure` is an error the server itself reported (nothing left to kill).
    let mut server_failed = false;
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
            // The OK packet was already received, so the statement itself succeeded even when
            // jumping to the next result set reports an error.
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
        let mut row_error: Option<Error> = None;
        loop {
            match result.next().await {
                Ok(Some(row)) => {
                    if ctl.stopped() {
                        failure = Some(Error::Cancelled);
                        break 'sets;
                    }
                    if o.max_rows.is_some_and(|m| count >= m) {
                        truncated = true; // discard the rest of this result set
                        continue;
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
                *statements += 1;
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
        *statements += 1;
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
    if failure.is_some() && !server_failed {
        // Stop the server from producing more data (no-op when it already finished).
        ctl.kill(inner, KillReason::Abort).await;
    }
    match tokio::time::timeout(DRAIN_TIMEOUT, result.drop_result()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            if e.is_fatal() {
                broken = true;
            }
            // An error announced by the server for a later statement surfaces only now; when we
            // are already failing (kill / sink error) it is just the echo of our own stop.
            if failure.is_none() {
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

/// MySQL / MariaDB session: a pool of connections to one server.
pub struct MySqlSession {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for MySqlSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MySqlSession")
            .field("is_mariadb", &self.inner.is_mariadb)
            .finish_non_exhaustive()
    }
}

impl MySqlSession {
    pub(crate) fn new(inner: Inner) -> Self {
        MySqlSession {
            inner: Arc::new(inner),
        }
    }

    /// Read-only check for `NO_BACKSLASH_ESCAPES` on this very connection.
    async fn no_backslash_escapes(conn: &mut Conn) -> Result<bool> {
        let mode: Option<String> = conn
            .query_first("SELECT @@SESSION.sql_mode")
            .await
            .map_err(map_query_error)?;
        Ok(mode.is_some_and(|m| m.to_ascii_uppercase().contains("NO_BACKSLASH_ESCAPES")))
    }

    async fn query_server_facts(conn: &mut Conn) -> Result<ServerInfo> {
        let row: Option<Row> = conn
            .query_first(
                "SELECT VERSION(), @@version_comment, @@character_set_server, @@collation_server, \
                 @@time_zone, @@system_time_zone, @@sql_mode, @@lower_case_table_names, \
                 @@max_allowed_packet",
            )
            .await
            .map_err(map_query_error)?;
        let row = row.ok_or_else(|| Error::Other("server returned no version".into()))?;
        let raw = cell(&row, 0).unwrap_or_default();
        let product = if detect_mariadb(&raw) {
            "MariaDB"
        } else {
            "MySQL"
        };
        let mut extra = BTreeMap::new();
        for (key, i) in [
            ("version_comment", 1),
            ("charset", 2),
            ("collation", 3),
            ("time_zone", 4),
            ("system_time_zone", 5),
            ("sql_mode", 6),
            ("lower_case_table_names", 7),
            ("max_allowed_packet", 8),
        ] {
            if let Some(v) = cell(&row, i) {
                extra.insert(key.to_string(), v);
            }
        }
        extra.insert("full_version".into(), raw.clone());
        let cipher: Option<Row> = conn
            .query_first("SHOW SESSION STATUS LIKE 'Ssl_cipher'")
            .await
            .map_err(map_query_error)?;
        let cipher = cipher.and_then(|r| cell(&r, 1)).unwrap_or_default();
        extra.insert(
            "tls".into(),
            if cipher.is_empty() { "off" } else { "on" }.into(),
        );
        if !cipher.is_empty() {
            extra.insert("tls_cipher".into(), cipher);
        }
        Ok(ServerInfo {
            product: product.into(),
            version: normalize_version(&raw),
            extra,
        })
    }

    /// Fetch facts through a connection of the pool (used by `connect` and `server_info`).
    pub(crate) async fn server_facts(&self) -> Result<ServerInfo> {
        let mut conn = self.inner.acquire().await?;
        let r = Self::query_server_facts(&mut conn).await;
        let broken = matches!(&r, Err(Error::Connection(_)));
        self.inner.release(conn, broken).await;
        r
    }
}

#[async_trait]
impl Session for MySqlSession {
    async fn ping(&self) -> Result<()> {
        let mut conn = self.inner.acquire().await?;
        let r = conn.ping().await.map_err(map_query_error);
        let broken = r.is_err();
        self.inner.release(conn, broken).await;
        r
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        self.server_facts().await
    }

    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        let started = Instant::now();
        let db = req.database.as_deref().filter(|d| !d.is_empty());
        let mut conn = self.inner.acquire_in(db).await?;
        let guard = self.inner.begin(conn.id(), req.query_id, req.timeout_ms);
        let mut statements = 0u32;
        let opts = StreamOpts {
            batch: req.batch_size.map_or(DEFAULT_BATCH, |b| b.max(1) as usize),
            max_rows: req.max_rows,
        };
        let (res, broken) = async {
            stream_query(
                &self.inner,
                &mut conn,
                &guard.ctl,
                &req.text,
                &opts,
                sink,
                &mut statements,
            )
            .await
        }
        .await;
        let ctl = guard.end().await;
        self.inner.release(conn, broken).await;
        ctl.finalize(res)?;
        Ok(ExecSummary {
            statements,
            elapsed_ms: elapsed_ms(started),
        })
    }

    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        let ctl = self
            .inner
            .running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&query_id)
            .cloned();
        // A query that already finished (or never started) is not an error: cancelling is
        // idempotent from the caller's point of view.
        if let Some(ctl) = ctl {
            ctl.kill(&self.inner, KillReason::Cancel).await;
        }
        Ok(())
    }

    async fn list_children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
        let mut conn = self.inner.acquire().await?;
        let r = browse::list_children(&mut conn, parent, self.inner.is_mariadb).await;
        self.inner.release(conn, is_conn_error(&r)).await;
        r
    }

    async fn object_ddl(&self, path: &ObjectPath) -> Result<String> {
        let mut conn = self.inner.acquire().await?;
        let r = browse::object_ddl(&mut conn, path).await;
        self.inner.release(conn, is_conn_error(&r)).await;
        r
    }

    async fn table_details(&self, path: &ObjectPath) -> Result<TableDetails> {
        let mut conn = self.inner.acquire().await?;
        let r = browse::table_details(&mut conn, path, self.inner.is_mariadb).await;
        self.inner.release(conn, is_conn_error(&r)).await;
        r
    }

    async fn fetch_table(
        &self,
        req: TableDataRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let started = Instant::now();
        let (db, table) = table_target(&req.table)?;
        let mut conn = self.inner.acquire().await?;
        let guard = self.inner.begin(conn.id(), req.query_id, None);
        let mut statements = 0u32;
        let opts = StreamOpts {
            batch: req.batch_size.map_or(DEFAULT_BATCH, |b| b.max(1) as usize),
            max_rows: None,
        };
        let (res, mut broken) = async {
            let nb = match Self::no_backslash_escapes(&mut conn).await {
                Ok(nb) => nb,
                Err(e) => return (Err(e), false),
            };
            if let Some(raw) = req.raw_where.as_deref() {
                if let Err(e) = check_raw_where(raw, nb) {
                    return (Err(e), false);
                }
            }
            let keys = match self.inner.key_columns(&mut conn, &db, &table).await {
                Ok(k) => k,
                Err(e) => return (Err(e), false),
            };
            let sql = match select_sql(&db, &table, &req, nb, &keys) {
                Ok(s) => s,
                Err(e) => return (Err(e), false),
            };
            // Let the server confirm this is exactly one statement before running it.
            if let Err(e) = prepare_single(&mut conn, &sql).await {
                return (Err(e), false);
            }
            if let Err(e) = conn.query_drop("START TRANSACTION READ ONLY").await {
                return (Err(map_query_error(e)), false);
            }
            stream_query(
                &self.inner,
                &mut conn,
                &guard.ctl,
                &sql,
                &opts,
                sink,
                &mut statements,
            )
            .await
        }
        .await;
        let ctl = guard.end().await;
        if !broken && conn.query_drop("ROLLBACK").await.is_err() {
            broken = true;
        }
        self.inner.release(conn, broken).await;
        ctl.finalize(res)?;
        Ok(ExecSummary {
            statements,
            elapsed_ms: elapsed_ms(started),
        })
    }

    async fn count_table(&self, req: TableCountRequest) -> Result<u64> {
        let (db, table) = table_target(&req.table)?;
        let mut conn = self.inner.acquire().await?;
        let guard = req.query_id.map(|id| self.inner.begin(conn.id(), id, None));
        let r = async {
            let nb = Self::no_backslash_escapes(&mut conn).await?;
            if let Some(raw) = req.raw_where.as_deref() {
                check_raw_where(raw, nb)?;
            }
            let sql = count_sql(&db, &table, &req, nb)?;
            prepare_single(&mut conn, &sql).await?;
            conn.query_drop("START TRANSACTION READ ONLY")
                .await
                .map_err(map_query_error)?;
            let row: Option<Row> = conn.query_first(sql).await.map_err(map_query_error)?;
            let n = row
                .and_then(|r| cell(&r, 0))
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| Error::Other("unexpected COUNT(*) result".into()))?;
            Ok(n)
        }
        .await;
        let ctl = match guard {
            Some(g) => Some(g.end().await),
            None => None,
        };
        let r = match &ctl {
            Some(c) => c.finalize(r),
            None => r,
        };
        let mut broken = is_conn_error(&r);
        if !broken && conn.query_drop("ROLLBACK").await.is_err() {
            broken = true;
        }
        self.inner.release(conn, broken).await;
        r
    }

    async fn close(&self) -> Result<()> {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // Stop statements that are still running; they end with `Cancelled`.
        let running: Vec<Arc<Ctl>> = self
            .inner
            .running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect();
        for ctl in &running {
            ctl.kill(&self.inner, KillReason::Cancel).await;
        }
        let pool = self.inner.pool.clone();
        match tokio::time::timeout(Duration::from_secs(5), pool.disconnect()).await {
            Ok(Ok(())) | Err(_) => Ok(()),
            Ok(Err(e)) => Err(map_query_error(e)),
        }
    }
}

/// Ask the server to prepare `sql`: `COM_STMT_PREPARE` rejects anything that is not exactly one
/// statement, which backs up the lexical checks on user-typed fragments.
async fn prepare_single(conn: &mut Conn, sql: &str) -> Result<()> {
    let stmt = conn.prep(sql).await.map_err(map_query_error)?;
    conn.close(stmt).await.map_err(map_query_error)
}

fn is_conn_error<T>(r: &Result<T>) -> bool {
    matches!(r, Err(Error::Connection(_) | Error::Timeout))
}
