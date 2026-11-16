//! Physical connections and the lazy per-database pools.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dbstudio_core::{Error, Result, Value};
use futures_util::TryStreamExt;
use tiberius::{CancelHandle, Client, QueryItem};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::error::{self, map_connect, map_query};
use crate::settings::{CATALOG_POOL_MAX, Settings};
use crate::value::{self, column_meta, int_at, str_at};
use crate::version::ServerVersion;

/// The `tiberius` client over a tokio TCP stream.
pub type TdsClient = Client<Compat<TcpStream>>;

/// Idle time after which a pooled connection is probed before reuse.
const PROBE_AFTER: Duration = Duration::from_secs(30);
/// Longest wait for a free pooled connection.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(60);

/// One result set of a catalog query.
pub type Rows = Vec<Vec<Value>>;

/// A failed connect attempt; `handshake_cause` is set when a TLS handshake was refused.
struct ConnectFailure {
    error: Error,
    handshake_cause: Option<String>,
}

/// A live connection.
pub struct Conn {
    pub client: TdsClient,
    pub cancel: CancelHandle,
    /// Database the connection was opened on (and the session is expected to be in).
    pub db: String,
    /// Set when the connection must not be reused.
    pub broken: bool,
    pub idle_since: Instant,
    /// Text/binary cell limit.
    pub lob_limit: usize,
    /// Session fingerprint right after login (see [`Conn::fingerprint`]).
    pub baseline: String,
    /// Server product version, read once after login.
    pub version: ServerVersion,
}

/// Fingerprint query. Built without `CONCAT` (2012+) so it also runs on 2008 R2: every part is
/// cast to text and joined with `+`.
fn fingerprint_sql() -> String {
    let parts = [
        "DB_NAME()",
        "@@OPTIONS",
        "s.language",
        "s.date_format",
        "s.date_first",
        "s.quoted_identifier",
        "s.arithabort",
        "s.ansi_null_dflt_on",
        "s.ansi_defaults",
        "s.ansi_warnings",
        "s.ansi_padding",
        "s.ansi_nulls",
        "s.concat_null_yields_null",
        "s.transaction_isolation_level",
        "s.lock_timeout",
        "s.deadlock_priority",
        "s.text_size",
        "s.row_count",
    ];
    let expr = parts
        .iter()
        .map(|p| format!("ISNULL(CAST({p} AS NVARCHAR(128)), N'')"))
        .collect::<Vec<_>>()
        .join(" + N'|' + ");
    format!("SELECT {expr}, @@TRANCOUNT FROM sys.dm_exec_sessions s WHERE s.session_id = @@SPID")
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conn")
            .field("db", &self.db)
            .field("broken", &self.broken)
            .finish_non_exhaustive()
    }
}

impl Conn {
    /// Open a connection to `db`.
    pub async fn open(settings: &Settings, db: &str) -> Result<Conn> {
        // ring and aws-lc-rs are both linked; rustls needs an explicit process default.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Self::connect_client(settings, db).await?;
        let cancel = client.cancel_handle();
        let mut conn = Conn {
            client,
            cancel,
            db: db.to_string(),
            broken: false,
            idle_since: Instant::now(),
            lob_limit: settings.max_lob_bytes,
            baseline: String::new(),
            version: ServerVersion::LATEST,
        };
        conn.version = conn.detect_version().await;
        conn.baseline = conn.fingerprint().await.map(|f| f.0).unwrap_or_default();
        Ok(conn)
    }

    /// `ProductVersion` of the server (newest assumed when unreadable). Under debug builds and
    /// tests `DBSTUDIO_MSSQL_FORCE_VERSION` overrides it so legacy SQL can run on a modern server.
    async fn detect_version(&mut self) -> ServerVersion {
        #[cfg(any(debug_assertions, test))]
        if let Some(v) = std::env::var("DBSTUDIO_MSSQL_FORCE_VERSION")
            .ok()
            .and_then(|s| ServerVersion::parse(&s))
        {
            return v;
        }
        match self
            .rows("SELECT CAST(SERVERPROPERTY('ProductVersion') AS NVARCHAR(128))")
            .await
        {
            Ok(rows) => rows.first().map_or(ServerVersion::LATEST, |r| {
                ServerVersion::parse_or_latest(&str_at(r, 0))
            }),
            Err(_) => ServerVersion::LATEST,
        }
    }

    /// TCP connect plus TDS login. With `tls_backend=auto` on Windows/macOS a TLS handshake that
    /// rustls cannot complete (SQL Server 2008 / 2008 R2, Windows Server 2012 R2 and older) is
    /// retried once on a fresh TCP connection with the operating system's TLS library, which is
    /// then used for the later connections too. Never falls back to plaintext or from strict.
    async fn connect_client(settings: &Settings, db: &str) -> Result<TdsClient> {
        let native_first = settings.use_native_now();
        let first = match Self::connect_attempt(settings, db, native_first).await {
            Ok(c) => return Ok(c),
            Err(f) => f,
        };
        let Some(cause) = first.handshake_cause else {
            return Err(first.error);
        };
        if native_first || !settings.can_fall_back_to_native() {
            return Err(error::handshake_error(
                &cause,
                crate::settings::native_tls_available() && !native_first,
                None,
            ));
        }
        tracing::warn!(
            "TLS handshake with {}:{} failed with rustls ({cause}); retrying with the operating system TLS stack",
            settings.host,
            settings.port
        );
        match Self::connect_attempt(settings, db, true).await {
            Ok(c) => {
                tracing::warn!(
                    "{}:{} needed the legacy operating system TLS stack (server only offers old TLS versions or cipher suites)",
                    settings.host,
                    settings.port
                );
                settings.remember_native();
                Ok(c)
            }
            Err(f) => match f.handshake_cause {
                Some(native_cause) => {
                    Err(error::handshake_error(&cause, false, Some(&native_cause)))
                }
                None => {
                    // TLS itself worked (the login or something later failed): keep using it.
                    if f.error.kind() == "authentication" {
                        settings.remember_native();
                    }
                    Err(f.error)
                }
            },
        }
    }

    /// One TCP connect and login attempt with the chosen TLS backend.
    async fn connect_attempt(
        settings: &Settings,
        db: &str,
        native: bool,
    ) -> std::result::Result<TdsClient, ConnectFailure> {
        let plain = |error| ConnectFailure {
            error,
            handshake_cause: None,
        };
        let config = settings.tiberius_config_for(db, native).map_err(plain)?;
        let addr = config.get_addr();
        let attempt = async {
            let tcp = TcpStream::connect(addr).await.map_err(|e| {
                plain(Error::Connection(format!(
                    "cannot connect to {}:{}: {e}",
                    settings.host, settings.port
                )))
            })?;
            let _ = tcp.set_nodelay(true);
            Client::connect(config, tcp.compat_write())
                .await
                .map_err(|e| ConnectFailure {
                    handshake_cause: error::is_handshake_failure(&e)
                        .then(|| error::handshake_cause(&e)),
                    error: map_connect(e),
                })
        };
        match tokio::time::timeout(settings.connect_timeout, attempt).await {
            Err(_) => Err(plain(Error::Timeout)),
            Ok(r) => r,
        }
    }

    /// Session settings that user statements can change and that must not leak into the pool:
    /// `(settings fingerprint, open transaction count)`. The fingerprint holds the current
    /// database, `@@OPTIONS`, language, date format/first, isolation level, lock timeout,
    /// deadlock priority, text size, row count and the ANSI/quoting flags.
    pub async fn fingerprint(&mut self) -> Result<(String, i64)> {
        let rows = self.rows(&fingerprint_sql()).await?;
        let r = rows.first().cloned().unwrap_or_default();
        Ok((str_at(&r, 0), int_at(&r, 1)))
    }

    /// True when the session still looks like a fresh login (nothing leaked).
    pub async fn is_clean(&mut self) -> bool {
        if self.broken {
            return false;
        }
        match tokio::time::timeout(Duration::from_secs(5), self.fingerprint()).await {
            Ok(Ok((fp, trancount))) => {
                if trancount > 0 {
                    let _ = self.run("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await;
                    return false;
                }
                fp == self.baseline
            }
            _ => false,
        }
    }

    /// Run a batch and collect every result set (catalog queries, small results).
    pub async fn sets(&mut self, sql: &str) -> Result<Vec<Rows>> {
        self.sets_once(sql).await.map_err(|(e, _)| e)
    }

    /// Like [`Conn::sets`] but for read-only queries: a deadlock victim (error 1205, possible
    /// between catalog reads and concurrent DDL) is retried.
    async fn sets_retrying(&mut self, sql: &str) -> Result<Vec<Rows>> {
        let mut attempt = 0;
        loop {
            match self.sets_once(sql).await {
                Ok(v) => return Ok(v),
                Err((_, Some(1205))) if attempt < 3 => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(50 * attempt)).await;
                }
                Err((e, _)) => return Err(e),
            }
        }
    }

    async fn sets_once(
        &mut self,
        sql: &str,
    ) -> std::result::Result<Vec<Rows>, (Error, Option<u32>)> {
        let lob = self.lob_limit;
        tracing::debug!(target: "dbstudio_mssql::query", len = sql.len(), head = %sql.chars().take(40).collect::<String>());
        let mut stream = match self.client.simple_query(sql).await {
            Ok(s) => s,
            Err(e) => {
                self.broken |= error::is_fatal(&e);
                let code = e.code();
                return Err((map_query(e, None), code));
            }
        };
        let mut sets: Vec<Rows> = Vec::new();
        let mut fatal = false;
        let mut cancelled = false;
        let result = async {
            while let Some(item) = stream.try_next().await? {
                match item {
                    QueryItem::Metadata(_) => sets.push(Vec::new()),
                    QueryItem::Row(row) => {
                        if sets.is_empty() {
                            sets.push(Vec::new());
                        }
                        let cells: Vec<Value> =
                            row.into_iter().map(|d| value::decode(d, lob)).collect();
                        if let Some(last) = sets.last_mut() {
                            last.push(cells);
                        }
                    }
                    QueryItem::Info(_) | QueryItem::Truncated(_) => {}
                    QueryItem::Done(d) => {
                        // A cancel that hit this internal query: its results are incomplete.
                        if d.attention {
                            cancelled = true;
                        }
                    }
                }
            }
            Ok::<(), tiberius::error::Error>(())
        }
        .await;
        drop(stream);
        match result {
            Ok(()) if cancelled => Err((Error::Cancelled, None)),
            Ok(()) => Ok(sets),
            Err(e) => {
                fatal |= error::is_fatal(&e);
                self.broken |= fatal;
                let code = e.code();
                Err((map_query(e, None), code))
            }
        }
    }

    /// First result set of a read-only batch (retried when chosen as a deadlock victim).
    pub async fn rows(&mut self, sql: &str) -> Result<Rows> {
        Ok(self
            .sets_retrying(sql)
            .await?
            .into_iter()
            .next()
            .unwrap_or_default())
    }

    /// Run a batch and ignore its results.
    pub async fn run(&mut self, sql: &str) -> Result<()> {
        self.sets(sql).await.map(|_| ())
    }

    /// Cheap liveness probe.
    pub async fn probe(&mut self) -> bool {
        matches!(
            tokio::time::timeout(Duration::from_secs(5), self.rows("SELECT 1")).await,
            Ok(Ok(_))
        )
    }

    /// Column metadata of a statement without running it (`sp_describe_first_result_set` is not
    /// used: the wire metadata of `SELECT TOP 0` is enough).
    #[allow(dead_code)]
    pub fn meta_of(&self, cols: &[tiberius::Column]) -> Vec<dbstudio_core::ColumnMeta> {
        cols.iter().map(column_meta).collect()
    }
}

struct Slot {
    idle: Mutex<Vec<Conn>>,
    sem: Arc<Semaphore>,
}

impl Slot {
    fn new(max: usize) -> Arc<Self> {
        Arc::new(Slot {
            idle: Mutex::new(Vec::new()),
            sem: Arc::new(Semaphore::new(max)),
        })
    }
}

struct DbPool {
    data: Arc<Slot>,
    catalog: Arc<Slot>,
}

/// A pooled connection; returns to the pool only through [`Pooled::release`].
pub struct Pooled {
    conn: Option<Conn>,
    slot: Arc<Slot>,
    _permit: OwnedSemaphorePermit,
}

impl std::fmt::Debug for Pooled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pooled").field("conn", &self.conn).finish()
    }
}

impl Deref for Pooled {
    type Target = Conn;
    fn deref(&self) -> &Conn {
        self.conn
            .as_ref()
            .expect("connection present until release")
    }
}

impl DerefMut for Pooled {
    fn deref_mut(&mut self) -> &mut Conn {
        self.conn
            .as_mut()
            .expect("connection present until release")
    }
}

impl Pooled {
    /// Give the connection back for reuse (dropped instead when broken).
    pub fn release(mut self) {
        if let Some(mut conn) = self.conn.take() {
            if !conn.broken {
                conn.idle_since = Instant::now();
                if let Ok(mut idle) = self.slot.idle.lock() {
                    idle.push(conn);
                }
            }
        }
    }
}

/// Settings plus the pools of every database touched.
pub struct Shared {
    pub settings: Settings,
    pools: Mutex<HashMap<String, Arc<DbPool>>>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("settings", &self.settings)
            .finish()
    }
}

impl Shared {
    pub fn new(settings: Settings) -> Self {
        Shared {
            settings,
            pools: Mutex::new(HashMap::new()),
        }
    }

    fn pool(&self, db: &str) -> Arc<DbPool> {
        let mut pools = self.pools.lock().expect("pool map lock");
        pools
            .entry(db.to_string())
            .or_insert_with(|| {
                Arc::new(DbPool {
                    data: Slot::new(self.settings.pool_max),
                    catalog: Slot::new(CATALOG_POOL_MAX),
                })
            })
            .clone()
    }

    /// A connection for user data work on `db`.
    pub async fn acquire(&self, db: &str) -> Result<Pooled> {
        let slot = self.pool(db).data.clone();
        self.acquire_slot(slot, db).await
    }

    /// A connection reserved for catalog queries on `db` (tree, search, details).
    pub async fn acquire_catalog(&self, db: &str) -> Result<Pooled> {
        let slot = self.pool(db).catalog.clone();
        self.acquire_slot(slot, db).await
    }

    async fn acquire_slot(&self, slot: Arc<Slot>, db: &str) -> Result<Pooled> {
        let permit =
            match tokio::time::timeout(ACQUIRE_TIMEOUT, slot.sem.clone().acquire_owned()).await {
                Ok(Ok(p)) => p,
                Ok(Err(_)) => return Err(Error::Other("connection pool closed".into())),
                Err(_) => return Err(Error::Busy("all connections are in use".into())),
            };
        loop {
            let candidate = slot.idle.lock().ok().and_then(|mut v| v.pop());
            let Some(mut conn) = candidate else { break };
            if conn.broken {
                continue;
            }
            if conn.idle_since.elapsed() > PROBE_AFTER && !conn.probe().await {
                continue;
            }
            return Ok(Pooled {
                conn: Some(conn),
                slot,
                _permit: permit,
            });
        }
        let conn = Conn::open(&self.settings, db).await?;
        Ok(Pooled {
            conn: Some(conn),
            slot,
            _permit: permit,
        })
    }

    /// Drop every idle connection.
    pub fn close_all(&self) {
        if let Ok(pools) = self.pools.lock() {
            for p in pools.values() {
                for slot in [&p.data, &p.catalog] {
                    if let Ok(mut idle) = slot.idle.lock() {
                        idle.clear();
                    }
                    slot.sem.close();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fingerprint_sql;

    #[test]
    fn fingerprint_avoids_concat() {
        let sql = fingerprint_sql();
        assert!(!sql.to_ascii_uppercase().contains("CONCAT("));
        assert!(sql.contains("sys.dm_exec_sessions"));
        assert!(sql.contains("@@TRANCOUNT"));
    }
}
