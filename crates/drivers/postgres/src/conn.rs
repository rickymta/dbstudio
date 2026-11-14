//! Connection establishment and a small per-database connection pool.

use std::collections::HashMap;
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use dbstudio_core::{ConnectionConfig, DbKind, Error, Result, Secret};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_postgres::config::SslMode;
use tokio_postgres::error::DbError;
use tokio_postgres::tls::{MakeTlsConnect, TlsConnect};
use tokio_postgres::{AsyncMessage, CancelToken, Client, NoTls, Socket};

use crate::error::{is_tls_error, map_connect_error, map_error};
use crate::flavor::Flavor;
use crate::tls::{self, TlsSetup};

/// Maximum simultaneous connections per database.
pub const MAX_CONNS_PER_DB: usize = 4;
/// Extra connections per database reserved for catalog queries (tree, DDL, details, ping), so
/// slow data fetches can never starve metadata browsing.
pub const CATALOG_CONNS_PER_DB: usize = 2;
/// Default server-side limit for `search_objects` (option `search_timeout_ms` overrides it).
const DEFAULT_SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What it takes to cancel the statement running on one connection: the protocol cancel key,
/// plus the server session id for CockroachDB (which ignores protocol cancel requests, so the
/// statement is cancelled by `CANCEL QUERIES` from another connection).
#[derive(Clone)]
pub struct CancelHandle {
    token: CancelToken,
    session_id: Option<Arc<str>>,
}

/// One physical connection plus the channel on which its server notices arrive.
pub struct Conn {
    pub client: Client,
    pub cancel: CancelHandle,
    notices: mpsc::UnboundedReceiver<DbError>,
    /// The user's script opened a transaction block that is still open (best effort tracking).
    pub in_txn: bool,
    /// The open transaction block is aborted (an error occurred; only ROLLBACK works).
    pub failed: bool,
    cursor_seq: u32,
}

impl Conn {
    /// Notices received since the last call, rendered as `SEVERITY: message`.
    pub fn drain_notices(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(n) = self.notices.try_recv() {
            out.push(format!("{}: {}", n.severity(), n.message()));
        }
        out
    }

    /// A cursor name that is unique on this connection.
    pub fn next_cursor_name(&mut self) -> String {
        self.cursor_seq += 1;
        format!("dbstudio_cur_{}", self.cursor_seq)
    }
}

struct Pool {
    idle: Mutex<Vec<Conn>>,
    permits: Arc<Semaphore>,
    catalog_permits: Arc<Semaphore>,
}

/// A connection checked out of the pool. It goes back to the pool only after
/// [`PooledConn::finish`] confirmed it is in a clean state; otherwise it is closed on drop.
pub struct PooledConn {
    conn: Option<Conn>,
    pool: Arc<Pool>,
    session_closed: Arc<AtomicBool>,
    reusable: bool,
    _permit: OwnedSemaphorePermit,
}

impl std::ops::Deref for PooledConn {
    type Target = Conn;
    fn deref(&self) -> &Conn {
        self.conn.as_ref().expect("connection present until drop")
    }
}

impl std::ops::DerefMut for PooledConn {
    fn deref_mut(&mut self) -> &mut Conn {
        self.conn.as_mut().expect("connection present until drop")
    }
}

impl PooledConn {
    /// Mark the connection reusable without any cleanup (for read-only catalog queries).
    pub fn mark_reusable(&mut self) {
        self.reusable = true;
    }

    /// Roll back an open transaction block, then mark the connection reusable.
    pub async fn finish(&mut self) {
        let conn = self.conn.as_mut().expect("connection present until drop");
        if conn.client.is_closed() {
            return;
        }
        if conn.in_txn {
            if conn.client.batch_execute("ROLLBACK").await.is_err() {
                return;
            }
            conn.in_txn = false;
        }
        self.reusable = true;
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else { return };
        if self.reusable && !conn.client.is_closed() && !self.session_closed.load(Ordering::Acquire)
        {
            if let Ok(mut idle) = self.pool.idle.lock() {
                idle.push(conn);
            }
        }
        // Otherwise `conn` is dropped here, which closes the connection.
    }
}

/// Shared state of a [`crate::PostgresSession`].
pub struct Shared {
    base: tokio_postgres::Config,
    tls: TlsSetup,
    pub login_db: String,
    connect_timeout: Duration,
    acquire_timeout: Duration,
    /// Server-side time limit of object searches.
    pub search_timeout: Duration,
    pools: Mutex<HashMap<String, Arc<Pool>>>,
    closed: Arc<AtomicBool>,
    server_timezone: OnceLock<String>,
    /// Kind chosen in the profile (decides defaults only; the flavour comes from the server).
    kind: DbKind,
    /// Flavour detected from `version()` on the first connection.
    flavor: OnceLock<Flavor>,
    /// The connection profile is read-only.
    pub read_only: bool,
}

impl Shared {
    /// Build the shared state from a connection config without touching the network.
    pub fn new(config: &ConnectionConfig, secret: Option<&Secret>) -> Result<Self> {
        let tls = tls::build(&config.tls)?;
        let mut base = tokio_postgres::Config::new();
        base.host(&config.host);
        base.port(config.port);
        base.user(
            config
                .username
                .as_deref()
                .unwrap_or(default_user(config.kind)),
        );
        if let Some(secret) = secret {
            base.password(secret.expose());
        }
        base.ssl_mode(tls.ssl_mode);
        base.application_name(
            config
                .options
                .get("application_name")
                .map(String::as_str)
                .unwrap_or("DBStudio"),
        );
        let mut startup = String::new();
        if config.read_only {
            startup.push_str("-c default_transaction_read_only=on ");
        }
        for (key, value) in &config.options {
            if key == "application_name" || key == "search_timeout_ms" {
                continue;
            }
            if key == "cockroach.cluster" {
                // CockroachDB Cloud routes the connection by cluster routing id.
                let id = value.trim();
                if id.is_empty()
                    || !id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    return Err(Error::Invalid(
                        "options.cockroach.cluster: invalid cluster routing id".into(),
                    ));
                }
                startup.push_str(&format!("--cluster={id} "));
                continue;
            }
            if key.is_empty()
                || !key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            {
                return Err(Error::Invalid(format!(
                    "invalid connection option name: {key:?}"
                )));
            }
            let escaped = value.replace('\\', "\\\\").replace(' ', "\\ ");
            startup.push_str(&format!("-c {key}={escaped} "));
        }
        if !startup.is_empty() {
            base.options(startup.trim_end());
        }
        let connect_timeout = config
            .connect_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
        Ok(Shared {
            base,
            tls,
            login_db: config
                .database
                .clone()
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| default_database(config.kind).to_string()),
            connect_timeout,
            acquire_timeout: config
                .connect_timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_ACQUIRE_TIMEOUT),
            search_timeout: config
                .options
                .get("search_timeout_ms")
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_SEARCH_TIMEOUT),
            pools: Mutex::new(HashMap::new()),
            closed: Arc::new(AtomicBool::new(false)),
            server_timezone: OnceLock::new(),
            kind: config.kind,
            flavor: OnceLock::new(),
            read_only: config.read_only,
        })
    }

    /// Time zone the server used before the driver switched the session to UTC.
    pub fn server_timezone(&self) -> Option<&str> {
        self.server_timezone.get().map(String::as_str)
    }

    /// Flavour of the server (the one chosen in the profile until the first connection told
    /// otherwise).
    pub fn flavor(&self) -> Flavor {
        self.flavor.get().copied().unwrap_or(match self.kind {
            DbKind::CockroachDb => Flavor::Cockroach { major: 0, minor: 0 },
            DbKind::YugabyteDb => Flavor::Yugabyte,
            _ => Flavor::Postgres,
        })
    }

    /// Kind chosen in the profile.
    pub fn profile_kind(&self) -> DbKind {
        self.kind
    }

    /// Server-side limit for catalog searches, widened for slow-catalog flavours.
    pub fn search_timeout(&self) -> Duration {
        self.search_timeout * self.flavor().timeout_factor()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Mark the session closed and drop every idle connection.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        if let Ok(pools) = self.pools.lock() {
            for pool in pools.values() {
                if let Ok(mut idle) = pool.idle.lock() {
                    idle.clear();
                }
                pool.permits.close();
                pool.catalog_permits.close();
            }
        }
    }

    /// Ask the server to cancel the statement running on the connection of `handle`.
    pub async fn cancel_token(&self, handle: &CancelHandle) -> Result<()> {
        if let Some(session_id) = handle.session_id.as_deref() {
            return self.cancel_by_session(session_id).await;
        }
        let result = match &self.tls.connector {
            Some(maker) => handle.token.cancel_query(maker.clone()).await,
            None => handle.token.cancel_query(NoTls).await,
        };
        result.map_err(|e| Error::Connection(format!("cancel request failed: {e}")))
    }

    /// CockroachDB: cancel the statements of a session from a pooled catalog connection.
    async fn cancel_by_session(&self, session_id: &str) -> Result<()> {
        // `session_id` comes from `SHOW session_id` (hex digits); never interpolate anything else.
        if session_id.is_empty() || !session_id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::Connection("invalid session id".into()));
        }
        // A dedicated connection with a short timeout: the cancel must neither wait for a
        // pool slot (all of them may be busy running the very statement to cancel) nor hold
        // the request's gate for long.
        let work = async {
            let conn = self.open_dedicated(&self.login_db).await?;
            // A cursor (`DECLARE ... FOR SELECT`) runs its query on an internal session that
            // shares the client address and has the application name prefixed with `$$ `:
            // cancel both. The address comes from the sessions list, so it is found while the
            // main session is idle between two FETCHes too.
            let sql = format!(
                "CANCEL QUERIES SELECT query_id FROM [SHOW CLUSTER STATEMENTS] \
                 WHERE user_name = current_user AND (session_id = '{session_id}' \
                    OR (left(application_name, 3) = '$$ ' AND client_address = \
                        (SELECT client_address FROM [SHOW CLUSTER SESSIONS] \
                         WHERE session_id = '{session_id}' LIMIT 1)))"
            );
            conn.client.batch_execute(&sql).await.map_err(map_error)
        };
        tokio::time::timeout(Duration::from_secs(5), work)
            .await
            .map_err(|_| Error::Timeout)?
    }

    fn pool_for(&self, db: &str) -> Arc<Pool> {
        let mut pools = self.pools.lock().expect("pool map poisoned");
        pools
            .entry(db.to_string())
            .or_insert_with(|| {
                Arc::new(Pool {
                    idle: Mutex::new(Vec::new()),
                    permits: Arc::new(Semaphore::new(MAX_CONNS_PER_DB)),
                    catalog_permits: Arc::new(Semaphore::new(CATALOG_CONNS_PER_DB)),
                })
            })
            .clone()
    }

    /// Check a connection for data work (queries, table pages) out of the pool, connecting
    /// lazily. Waits at most the acquire timeout, then fails with [`Error::Timeout`].
    pub async fn acquire(&self, db: &str) -> Result<PooledConn> {
        self.acquire_kind(db, false).await
    }

    /// Like [`Shared::acquire`] but from the reserved catalog slots.
    pub async fn acquire_catalog(&self, db: &str) -> Result<PooledConn> {
        self.acquire_kind(db, true).await
    }

    async fn acquire_kind(&self, db: &str, catalog: bool) -> Result<PooledConn> {
        if self.is_closed() {
            return Err(Error::Connection("session is closed".to_string()));
        }
        let pool = self.pool_for(db);
        let sem = if catalog {
            pool.catalog_permits.clone()
        } else {
            pool.permits.clone()
        };
        let permit = tokio::time::timeout(self.acquire_timeout, sem.acquire_owned())
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|_| Error::Connection("session is closed".to_string()))?;
        let reused = {
            let mut idle = pool.idle.lock().expect("idle list poisoned");
            let mut found = None;
            // Most recently used first so sequential calls keep the same server session.
            while let Some(c) = idle.pop() {
                if !c.client.is_closed() {
                    found = Some(c);
                    break;
                }
            }
            found
        };
        let conn = match reused {
            Some(c) => c,
            None => self.connect(db).await?,
        };
        Ok(PooledConn {
            conn: Some(conn),
            pool,
            session_closed: self.closed.clone(),
            reusable: false,
            _permit: permit,
        })
    }

    /// Open a connection outside the pool for a lease: not counted against the pool limits and
    /// never returned to it. The caller owns it and closing it is just dropping it.
    pub async fn open_dedicated(&self, db: &str) -> Result<Conn> {
        if self.is_closed() {
            return Err(Error::Connection("session is closed".to_string()));
        }
        self.connect(db).await
    }

    /// Open a new connection to `db`, honouring the TLS mode and connect timeout.
    async fn connect(&self, db: &str) -> Result<Conn> {
        let attempt = async {
            let mut cfg = self.base.clone();
            cfg.dbname(db);
            let first = match &self.tls.connector {
                Some(maker) => open(&cfg, maker.clone()).await,
                None => open(&cfg, NoTls).await,
            };
            match first {
                Err(e) if self.tls.ssl_mode == SslMode::Prefer && is_tls_error(&e) => {
                    // libpq `prefer`: fall back to an unencrypted connection.
                    cfg.ssl_mode(SslMode::Disable);
                    open(&cfg, NoTls).await
                }
                other => other,
            }
            .map_err(map_connect_error)
        };
        let conn = tokio::time::timeout(self.connect_timeout, attempt)
            .await
            .map_err(|_| Error::Timeout)??;
        let mut conn = conn;
        self.init_session(&conn).await?;
        if self.flavor().is_cockroach() {
            // Protocol cancel requests do not work on CockroachDB: remember the session id.
            let row = conn
                .client
                .query_one("SHOW session_id", &[])
                .await
                .map_err(map_error)?;
            conn.cancel.session_id = Some(Arc::from(row.get::<_, String>(0)));
        }
        Ok(conn)
    }

    async fn init_session(&self, conn: &Conn) -> Result<()> {
        if self.server_timezone.get().is_none() {
            let row = conn
                .client
                .query_one("SELECT current_setting('TimeZone')", &[])
                .await
                .map_err(map_error)?;
            let _ = self.server_timezone.set(row.get::<_, String>(0));
        }
        if self.flavor.get().is_none() {
            let row = conn
                .client
                .query_one("SELECT version()", &[])
                .await
                .map_err(map_error)?;
            let _ = self.flavor.set(Flavor::detect(&row.get::<_, String>(0)));
        }
        conn.client
            .batch_execute(
                "SET TIME ZONE 'UTC'; SET extra_float_digits = 3; SET DateStyle = 'ISO, MDY'; \
                 SET bytea_output = 'hex'",
            )
            .await
            .map_err(map_error)
    }
}

fn default_user(kind: DbKind) -> &'static str {
    match kind {
        DbKind::CockroachDb => "root",
        DbKind::YugabyteDb => "yugabyte",
        _ => "postgres",
    }
}

fn default_database(kind: DbKind) -> &'static str {
    match kind {
        DbKind::CockroachDb => "defaultdb",
        DbKind::YugabyteDb => "yugabyte",
        _ => "postgres",
    }
}

async fn open<T>(
    cfg: &tokio_postgres::Config,
    tls: T,
) -> std::result::Result<Conn, tokio_postgres::Error>
where
    T: MakeTlsConnect<Socket> + Send,
    T::Stream: Send + 'static,
    T::TlsConnect: Send,
    <T::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    let (client, mut connection) = cfg.connect(tls).await?;
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match poll_fn(|cx| connection.poll_message(cx)).await {
                Some(Ok(AsyncMessage::Notice(n))) => {
                    let _ = tx.send(n);
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            }
        }
    });
    let cancel = CancelHandle {
        token: client.cancel_token(),
        session_id: None,
    };
    Ok(Conn {
        client,
        cancel,
        notices: rx,
        in_txn: false,
        failed: false,
        cursor_seq: 0,
    })
}

macro_rules! opaque_debug {
    ($($t:ty),*) => {$(
        impl std::fmt::Debug for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($t)).finish_non_exhaustive()
            }
        }
    )*};
}
opaque_debug!(Conn, Pool, PooledConn, Shared);
