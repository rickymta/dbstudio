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
use crate::value::{self, column_meta};

/// The `tiberius` client over a tokio TCP stream.
pub type TdsClient = Client<Compat<TcpStream>>;

/// Idle time after which a pooled connection is probed before reuse.
const PROBE_AFTER: Duration = Duration::from_secs(30);
/// Longest wait for a free pooled connection.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(60);

/// One result set of a catalog query.
pub type Rows = Vec<Vec<Value>>;

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
        let config = settings.tiberius_config(db)?;
        let addr = config.get_addr();
        let attempt = async {
            let tcp = TcpStream::connect(addr).await.map_err(|e| {
                Error::Connection(format!(
                    "cannot connect to {}:{}: {e}",
                    settings.host, settings.port
                ))
            })?;
            let _ = tcp.set_nodelay(true);
            Client::connect(config, tcp.compat_write())
                .await
                .map_err(map_connect)
        };
        let client = match tokio::time::timeout(settings.connect_timeout, attempt).await {
            Err(_) => return Err(Error::Timeout),
            Ok(r) => r?,
        };
        let cancel = client.cancel_handle();
        Ok(Conn {
            client,
            cancel,
            db: db.to_string(),
            broken: false,
            idle_since: Instant::now(),
            lob_limit: settings.max_lob_bytes,
        })
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
                    QueryItem::Info(_) | QueryItem::Done(_) => {}
                }
            }
            Ok::<(), tiberius::error::Error>(())
        }
        .await;
        drop(stream);
        match result {
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
