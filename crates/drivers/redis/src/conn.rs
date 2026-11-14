//! Connection setup (TLS modes, ACL user, database) and the per-database connection cache.
//!
//! Limitation (R6): the `redis` crate connects to and verifies the certificate against the same
//! `host`, so `tls.server_name` is not honoured. Behind an SSH tunnel the host is `127.0.0.1`;
//! `VerifyFull`/`VerifyCa` then need a certificate that lists that address.

use std::collections::HashMap;
use std::time::Duration;

use dbstudio_core::{ConnectionConfig, Error, Result, Secret, TlsMode};
use redis::aio::MultiplexedConnection;
use redis::{
    AsyncConnectionConfig, Client, Cmd, ConnectionAddr, ConnectionInfo, FromRedisValue,
    IntoConnectionInfo, RedisConnectionInfo, RedisError, TlsCertificates,
};
use tokio::sync::Mutex as AsyncMutex;

use crate::error::map_err;

/// Default connect timeout.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// State shared by the session, its leases and the commands.
pub struct Shared {
    pub client: Client,
    pub read_only: bool,
    pub default_db: i64,
    pub delimiter: String,
    pub show_empty_dbs: bool,
    pub connect_timeout: Duration,
    conns: AsyncMutex<HashMap<i64, MultiplexedConnection>>,
    /// Lower-cased command name -> flags from `COMMAND INFO` (`None` = unknown command).
    pub cmd_flags: std::sync::Mutex<HashMap<String, Option<Vec<String>>>>,
    /// Running statements of `Session::execute`: query id -> (client id, cancelled flag).
    pub running: std::sync::Mutex<HashMap<uuid::Uuid, Running>>,
}

/// A statement of `Session::execute` that can be cancelled.
#[derive(Clone)]
pub struct Running {
    pub client_id: i64,
    pub cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("default_db", &self.default_db)
            .field("read_only", &self.read_only)
            .finish_non_exhaustive()
    }
}

/// Database number of a config / option value (`3` or `db3`).
pub fn parse_db(text: &str) -> Option<i64> {
    let t = text.trim();
    t.strip_prefix("db")
        .unwrap_or(t)
        .parse::<i64>()
        .ok()
        .filter(|n| *n >= 0)
}

fn truthy(v: Option<&String>) -> bool {
    v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn read_file(path: &str, what: &str) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| Error::Tls(format!("cannot read {what} `{path}`: {e}")))
}

fn build_client(
    cfg: &ConnectionConfig,
    secret: Option<&Secret>,
    use_tls: bool,
    db: i64,
) -> Result<Client> {
    let mut settings = RedisConnectionInfo::default().set_db(db);
    if let Some(user) = cfg.username.as_deref().filter(|u| !u.is_empty()) {
        settings = settings.set_username(user);
    }
    if let Some(pw) = secret.map(Secret::expose).filter(|p| !p.is_empty()) {
        settings = settings.set_password(pw);
    }
    let invalid = |e: RedisError| Error::Connection(e.to_string());
    if !use_tls {
        let info = ConnectionAddr::Tcp(cfg.host.clone(), cfg.port)
            .into_connection_info()
            .map_err(invalid)?
            .set_redis_settings(settings);
        return Client::open(info).map_err(invalid);
    }
    let verify = matches!(cfg.tls.mode, TlsMode::VerifyCa | TlsMode::VerifyFull);
    let addr = ConnectionAddr::TcpTls {
        host: cfg.host.clone(),
        port: cfg.port,
        insecure: !verify,
        tls_params: None,
    };
    let info = addr
        .into_connection_info()
        .map_err(invalid)?
        .set_redis_settings(settings);
    let ca = cfg
        .tls
        .ca_cert_path
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(|p| read_file(p, "CA certificate"))
        .transpose()?;
    let client_tls = match (
        cfg.tls
            .client_cert_path
            .as_deref()
            .filter(|p| !p.is_empty()),
        cfg.tls.client_key_path.as_deref().filter(|p| !p.is_empty()),
    ) {
        (Some(cert), Some(key)) => Some(redis::ClientTlsConfig {
            client_cert: read_file(cert, "client certificate")?,
            client_key: read_file(key, "client key")?,
        }),
        _ => None,
    };
    if ca.is_none() && client_tls.is_none() {
        return Client::open(info).map_err(invalid);
    }
    Client::build_with_tls(
        info,
        TlsCertificates {
            client_tls,
            root_cert: ca,
        },
    )
    .map_err(|e| Error::Tls(e.to_string()))
}

async fn open_with(
    client: &Client,
    timeout: Duration,
) -> std::result::Result<MultiplexedConnection, RedisError> {
    let cfg = AsyncConnectionConfig::new()
        .set_connection_timeout(Some(timeout))
        .set_response_timeout(None);
    client
        .get_multiplexed_async_connection_with_config(&cfg)
        .await
}

impl Shared {
    /// Build the client (resolving the TLS mode) and verify the credentials with a first
    /// connection.
    pub async fn connect(cfg: &ConnectionConfig, secret: Option<&Secret>) -> Result<Shared> {
        // ring and aws-lc-rs are both linked; rustls needs an explicit process default.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let default_db = cfg.database.as_deref().and_then(parse_db).unwrap_or(0);
        let timeout = cfg
            .connect_timeout_ms
            .map_or(DEFAULT_CONNECT_TIMEOUT, Duration::from_millis);
        let (client, first) = match cfg.tls.mode {
            TlsMode::Disable => {
                let c = build_client(cfg, secret, false, default_db)?;
                let first = open_with(&c, timeout).await.map_err(map_err)?;
                (c, first)
            }
            TlsMode::Prefer => {
                let tls = build_client(cfg, secret, true, default_db)?;
                match open_with(&tls, timeout).await {
                    Ok(conn) => (tls, conn),
                    Err(tls_err) => {
                        // Prefer: fall back to plain text (documented downgrade).
                        let plain = build_client(cfg, secret, false, default_db)?;
                        match open_with(&plain, timeout).await {
                            Ok(conn) => (plain, conn),
                            // The plain attempt failed too: report the plain error unless it
                            // is a protocol mismatch, in which case the TLS error is the clue.
                            Err(plain_err) => {
                                let err = if plain_err.is_connection_refusal() {
                                    plain_err
                                } else {
                                    tls_err
                                };
                                return Err(map_err(err));
                            }
                        }
                    }
                }
            }
            _ => {
                let c = build_client(cfg, secret, true, default_db)?;
                let first = open_with(&c, timeout).await.map_err(map_err)?;
                (c, first)
            }
        };
        let mut conns = HashMap::new();
        conns.insert(default_db, first);
        let options = &cfg.options;
        let shared = Shared {
            client,
            read_only: cfg.read_only,
            default_db,
            delimiter: options
                .get("delimiter")
                .filter(|d| !d.is_empty())
                .cloned()
                .unwrap_or_else(|| ":".to_string()),
            show_empty_dbs: truthy(options.get("show_empty_dbs"))
                || truthy(options.get("showEmptyDbs")),
            connect_timeout: timeout,
            conns: AsyncMutex::new(conns),
            cmd_flags: std::sync::Mutex::new(HashMap::new()),
            running: std::sync::Mutex::new(HashMap::new()),
        };
        // Validate the ACL / password now so a bad login fails at connect time.
        shared
            .run::<String>(default_db, &redis::cmd("PING"))
            .await?;
        Ok(shared)
    }

    /// Client for database `db`, derived from the connected client (TLS parameters kept).
    fn client_for(&self, db: i64) -> Result<Client> {
        if db == self.default_db {
            return Ok(self.client.clone());
        }
        let info: ConnectionInfo = self.client.get_connection_info().clone();
        let settings = info.redis_settings().clone().set_db(db);
        Client::open(info.set_redis_settings(settings))
            .map_err(|e| Error::Connection(e.to_string()))
    }

    /// A new dedicated connection (not shared) to `db`. Used by leases, scripts and edits.
    pub async fn dedicated(&self, db: i64) -> Result<MultiplexedConnection> {
        let client = self.client_for(db)?;
        open_with(&client, self.connect_timeout)
            .await
            .map_err(map_err)
    }

    /// The cached shared connection of `db` (created lazily).
    async fn shared_conn(&self, db: i64) -> Result<MultiplexedConnection> {
        let mut conns = self.conns.lock().await;
        if let Some(c) = conns.get(&db) {
            return Ok(c.clone());
        }
        let c = self.dedicated(db).await?;
        conns.insert(db, c.clone());
        Ok(c)
    }

    async fn forget(&self, db: i64) {
        self.conns.lock().await.remove(&db);
    }

    /// Run one command on the shared connection of `db`; reconnects once when the connection
    /// was dropped.
    pub async fn run<T: FromRedisValue>(&self, db: i64, cmd: &Cmd) -> Result<T> {
        let mut conn = self.shared_conn(db).await?;
        match cmd.query_async::<T>(&mut conn).await {
            Ok(v) => Ok(v),
            Err(e) if e.is_connection_dropped() || e.is_io_error() => {
                self.forget(db).await;
                let mut conn = self.shared_conn(db).await?;
                cmd.query_async::<T>(&mut conn).await.map_err(map_err)
            }
            Err(e) => Err(map_err(e)),
        }
    }

    /// Run a pipeline (not atomic) on the shared connection of `db`.
    pub async fn run_pipe<T: FromRedisValue>(&self, db: i64, pipe: &redis::Pipeline) -> Result<T> {
        let mut conn = self.shared_conn(db).await?;
        match pipe.query_async::<T>(&mut conn).await {
            Ok(v) => Ok(v),
            Err(e) if e.is_connection_dropped() || e.is_io_error() => {
                self.forget(db).await;
                let mut conn = self.shared_conn(db).await?;
                pipe.query_async::<T>(&mut conn).await.map_err(map_err)
            }
            Err(e) => Err(map_err(e)),
        }
    }

    /// Drop every cached connection.
    pub async fn close(&self) {
        self.conns.lock().await.clear();
    }
}
