//! Connection setup (TLS modes, ACL user, database) and the per-database connection cache.
//!
//! R6: behind an SSH tunnel the profile host is `127.0.0.1:<tunnel port>` and `tls.server_name`
//! holds the original host. The client is then built for the ORIGINAL host (SNI and certificate
//! name check) and a custom DNS resolver maps that name to the tunnel address.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use dbstudio_core::{ConnectionConfig, Error, Result, Secret, TlsMode};
use redis::aio::MultiplexedConnection;
use redis::{
    AsyncConnectionConfig, Client, Cmd, ConnectionAddr, ConnectionInfo, FromRedisValue,
    IntoConnectionInfo, RedisConnectionInfo, RedisError, TlsCertificates,
};
use tokio::sync::Mutex as AsyncMutex;

use crate::cluster::{self, Redirect, SlotRange};
use crate::error::map_err;

/// Resolver that sends `logical` to the tunnel address and everything else to the system DNS.
#[derive(Clone, Debug)]
pub struct TunnelResolver {
    logical: String,
    target_host: String,
}

impl redis::io::AsyncDNSResolver for TunnelResolver {
    fn resolve<'a, 'b: 'a>(
        &'a self,
        host: &'b str,
        port: u16,
    ) -> redis::RedisFuture<'a, Box<dyn Iterator<Item = SocketAddr> + Send + 'a>> {
        let name = if host.eq_ignore_ascii_case(&self.logical) {
            self.target_host.clone()
        } else {
            host.to_string()
        };
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), port))
                .await
                .map_err(redis::RedisError::from)?
                .collect();
            Ok(Box::new(addrs.into_iter()) as Box<dyn Iterator<Item = SocketAddr> + Send + 'a>)
        })
    }
}

/// Name to use for TLS (SNI + certificate check) when it differs from the connect address.
fn tls_logical_host(cfg: &ConnectionConfig) -> Option<String> {
    cfg.tls
        .server_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case(&cfg.host))
        .map(str::to_string)
}

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
    /// Custom name resolution for TLS behind an SSH tunnel (R6); `None` when not needed.
    resolver: Option<Arc<TunnelResolver>>,
    conns: AsyncMutex<HashMap<i64, MultiplexedConnection>>,
    /// Lower-cased command name -> flags from `COMMAND INFO` (`None` = unknown command).
    pub cmd_flags: std::sync::Mutex<HashMap<String, Option<Vec<String>>>>,
    /// Running statements of `Session::execute`: query id -> (client id, cancelled flag).
    pub running: std::sync::Mutex<HashMap<uuid::Uuid, Running>>,
    /// Redis Cluster routing (`options.topology = cluster`); `None` for a single server.
    pub cluster: Option<ClusterState>,
    /// `standalone`, `sentinel` or `cluster` (shown in the server info).
    pub topology: &'static str,
    /// `host:port` of the server this session landed on (the master Sentinel named, the first
    /// cluster node that answered).
    pub endpoint: String,
}

/// Slot map and per-primary connections of a Redis Cluster.
pub struct ClusterState {
    ranges: std::sync::RwLock<Vec<SlotRange>>,
    conns: AsyncMutex<HashMap<String, MultiplexedConnection>>,
}

impl ClusterState {
    fn ranges(&self) -> Vec<SlotRange> {
        self.ranges
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Topologies of `options.topology` this driver understands.
const STANDALONE: &str = "standalone";
const SENTINEL: &str = "sentinel";
const CLUSTER: &str = "cluster";
/// `options` key: name of the master the sentinels watch.
pub const SENTINEL_MASTER: &str = "sentinelMaster";

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
    if let Some(user) = cfg.credential_user() {
        settings = settings.set_username(user);
    }
    if let Some(pw) = cfg
        .effective_secret(secret)
        .map(Secret::expose)
        .filter(|p| !p.is_empty())
    {
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
        host: tls_logical_host(cfg).unwrap_or_else(|| cfg.host.clone()),
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
    resolver: Option<&TunnelResolver>,
) -> std::result::Result<MultiplexedConnection, RedisError> {
    let mut cfg = AsyncConnectionConfig::new()
        .set_connection_timeout(Some(timeout))
        .set_response_timeout(None);
    if let Some(r) = resolver {
        cfg = cfg.set_dns_resolver(r.clone());
    }
    client
        .get_multiplexed_async_connection_with_config(&cfg)
        .await
}

impl Shared {
    /// Connect as the profile's topology says: one server, the master named by Sentinel, or a
    /// Redis Cluster.
    pub async fn connect(cfg: &ConnectionConfig, secret: Option<&Secret>) -> Result<Shared> {
        // ring and aws-lc-rs are both linked; rustls needs an explicit process default.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let topology = cfg.topology();
        match topology.as_str() {
            "" | STANDALONE => Self::connect_standalone(cfg, secret).await,
            SENTINEL | CLUSTER if cfg.ssh.is_some() => Err(Error::Unsupported(format!(
                "Redis {topology} cannot go through an SSH tunnel: the servers it names have \
                 addresses only the network behind the tunnel can reach"
            ))),
            SENTINEL => {
                let (host, port) = sentinel_master(cfg, secret).await?;
                let mut master = cfg.clone();
                master.host = host;
                master.port = port;
                let mut shared = Self::connect_standalone(&master, secret).await?;
                shared.topology = SENTINEL;
                Ok(shared)
            }
            CLUSTER => Self::connect_cluster(cfg, secret).await,
            other => Err(Error::Invalid(format!(
                "options.topology: expected `standalone`, `sentinel` or `cluster`, got {other:?}"
            ))),
        }
    }

    /// Redis Cluster: the first listed node that answers gives the slot map.
    async fn connect_cluster(cfg: &ConnectionConfig, secret: Option<&Secret>) -> Result<Shared> {
        if cfg.database.as_deref().and_then(parse_db).unwrap_or(0) != 0 {
            return Err(Error::Invalid(
                "a Redis Cluster has only database 0: clear the database of the profile".into(),
            ));
        }
        let nodes = cfg.nodes();
        let mut last = None;
        for (i, (host, port)) in nodes.iter().enumerate() {
            let mut node = cfg.clone();
            node.host = host.clone();
            node.port = *port;
            node.database = None;
            let mut shared = match Self::connect_standalone(&node, secret).await {
                Ok(s) => s,
                Err(e @ (Error::Connection(_) | Error::Timeout | Error::Io(_)))
                    if i + 1 < nodes.len() =>
                {
                    last = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            };
            let seed = cluster::node_addr(host, *port);
            let reply: redis::Value = shared
                .run(0, redis::cmd("CLUSTER").arg("SLOTS"))
                .await
                .map_err(|e| match e {
                    Error::Query { message, .. }
                        if message.contains("cluster support disabled") =>
                    {
                        Error::Invalid(format!(
                            "{seed} is not part of a Redis Cluster (cluster-enabled is off): \
                             use the standalone topology"
                        ))
                    }
                    other => other,
                })?;
            let ranges = cluster::parse_slots(&reply, &seed);
            if ranges.is_empty() {
                return Err(Error::Connection(format!(
                    "{seed} answered CLUSTER SLOTS without any slot: the cluster is not ready"
                )));
            }
            let mut conns = HashMap::new();
            if let Some(first) = shared.conns.lock().await.get(&0).cloned() {
                conns.insert(seed, first);
            }
            shared.cluster = Some(ClusterState {
                ranges: std::sync::RwLock::new(ranges),
                conns: AsyncMutex::new(conns),
            });
            shared.topology = CLUSTER;
            return Ok(shared);
        }
        Err(last.unwrap_or_else(|| Error::Connection("no server to connect to".into())))
    }

    /// One server (also each node of a cluster): resolve the TLS mode, verify the credentials.
    async fn connect_standalone(cfg: &ConnectionConfig, secret: Option<&Secret>) -> Result<Shared> {
        let default_db = cfg.database.as_deref().and_then(parse_db).unwrap_or(0);
        let timeout = cfg
            .connect_timeout_ms
            .map_or(DEFAULT_CONNECT_TIMEOUT, Duration::from_millis);
        let tls_resolver = tls_logical_host(cfg).map(|logical| TunnelResolver {
            logical,
            target_host: cfg.host.clone(),
        });
        // A resolver only applies to TLS clients, whose address carries the logical name.
        let r = tls_resolver.as_ref();
        let (client, first, use_resolver) = match cfg.tls.mode {
            TlsMode::Disable => {
                let c = build_client(cfg, secret, false, default_db)?;
                let first = open_with(&c, timeout, None).await.map_err(map_err)?;
                (c, first, false)
            }
            TlsMode::Prefer => {
                let tls = build_client(cfg, secret, true, default_db)?;
                match open_with(&tls, timeout, r).await {
                    Ok(conn) => (tls, conn, true),
                    Err(tls_err) => {
                        // Prefer: fall back to plain text (documented downgrade).
                        let plain = build_client(cfg, secret, false, default_db)?;
                        match open_with(&plain, timeout, None).await {
                            Ok(conn) => (plain, conn, false),
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
                let first = open_with(&c, timeout, r).await.map_err(map_err)?;
                (c, first, true)
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
            resolver: tls_resolver.filter(|_| use_resolver).map(Arc::new),
            conns: AsyncMutex::new(conns),
            cmd_flags: std::sync::Mutex::new(HashMap::new()),
            running: std::sync::Mutex::new(HashMap::new()),
            cluster: None,
            topology: STANDALONE,
            endpoint: cluster::node_addr(&cfg.host, cfg.port),
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
        open_with(&client, self.connect_timeout, self.resolver.as_deref())
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
    /// was dropped. In a cluster the command goes to the primary owning its key.
    pub async fn run<T: FromRedisValue>(&self, db: i64, cmd: &Cmd) -> Result<T> {
        if let Some(c) = &self.cluster {
            let reply = self.cluster_query(c, cmd).await?;
            return redis::from_redis_value(reply).map_err(|e| map_err(e.into()));
        }
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

    /// Run a pipeline (not atomic) on the shared connection of `db`. In a cluster its commands
    /// may belong to different primaries: each one is routed on its own.
    pub async fn run_pipe<T: FromRedisValue>(&self, db: i64, pipe: &redis::Pipeline) -> Result<T> {
        if let Some(c) = &self.cluster {
            let mut replies = Vec::new();
            for cmd in pipe.cmd_iter() {
                replies.push(self.cluster_query(c, cmd).await?);
            }
            return redis::from_redis_value(redis::Value::Array(replies))
                .map_err(|e| map_err(e.into()));
        }
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
        if let Some(c) = &self.cluster {
            c.conns.lock().await.clear();
        }
    }

    /// Number of primaries of the cluster (0 for a single server).
    pub fn primary_count(&self) -> usize {
        self.cluster
            .as_ref()
            .map_or(0, |c| cluster::primaries(&c.ranges()).len())
    }

    /// True for a Redis Cluster session.
    pub fn is_cluster(&self) -> bool {
        self.cluster.is_some()
    }

    /// In a cluster, the connection of the primary owning `key`; `None` for a single server.
    pub async fn node_conn_for_key(&self, key: &[u8]) -> Result<Option<MultiplexedConnection>> {
        let Some(c) = &self.cluster else {
            return Ok(None);
        };
        let addr = self.node_of(c, key);
        Ok(Some(self.node_conn(c, &addr).await?))
    }

    /// Connections to every primary of the cluster (empty for a single server).
    pub async fn primary_conns(&self) -> Result<Vec<MultiplexedConnection>> {
        let Some(c) = &self.cluster else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for addr in cluster::primaries(&c.ranges()) {
            out.push(self.node_conn(c, &addr).await?);
        }
        Ok(out)
    }

    fn node_of(&self, c: &ClusterState, key: &[u8]) -> String {
        let ranges = c.ranges();
        cluster::node_for(&ranges, cluster::slot(key))
            .map(str::to_string)
            .or_else(|| ranges.first().map(|r| r.node.clone()))
            .unwrap_or_default()
    }

    /// Client for a cluster node: the connected client's settings (TLS, login) at `addr`.
    fn client_at(&self, addr: &str) -> Result<Client> {
        let (host, port) = cluster::split_addr(addr)
            .ok_or_else(|| Error::Connection(format!("invalid cluster node address `{addr}`")))?;
        let info: ConnectionInfo = self.client.get_connection_info().clone();
        let at = match info.addr().clone() {
            ConnectionAddr::TcpTls {
                insecure,
                tls_params,
                ..
            } => ConnectionAddr::TcpTls {
                host,
                port,
                insecure,
                tls_params,
            },
            _ => ConnectionAddr::Tcp(host, port),
        };
        Client::open(info.set_addr(at)).map_err(|e| Error::Connection(e.to_string()))
    }

    async fn node_conn(&self, c: &ClusterState, addr: &str) -> Result<MultiplexedConnection> {
        let mut conns = c.conns.lock().await;
        if let Some(conn) = conns.get(addr) {
            return Ok(conn.clone());
        }
        let client = self.client_at(addr)?;
        let conn = open_with(&client, self.connect_timeout, None)
            .await
            .map_err(map_err)?;
        conns.insert(addr.to_string(), conn.clone());
        Ok(conn)
    }

    /// Read the slot map again (after a `MOVED` or a lost node).
    async fn refresh_slots(&self, c: &ClusterState) {
        for addr in cluster::primaries(&c.ranges()) {
            let Ok(mut conn) = self.node_conn(c, &addr).await else {
                c.conns.lock().await.remove(&addr);
                continue;
            };
            if let Ok(reply) = redis::cmd("CLUSTER")
                .arg("SLOTS")
                .query_async::<redis::Value>(&mut conn)
                .await
            {
                let ranges = cluster::parse_slots(&reply, &addr);
                if !ranges.is_empty() {
                    *c.ranges.write().unwrap_or_else(|e| e.into_inner()) = ranges;
                    return;
                }
            }
        }
    }

    /// One command on the cluster: routed by key, answered for the whole cluster where the
    /// command is about every key (`DBSIZE`, `INFO keyspace`, `SCAN`), redirections followed.
    async fn cluster_query(&self, c: &ClusterState, cmd: &Cmd) -> Result<redis::Value> {
        use redis::Value as V;
        let args = cmd_args(cmd);
        let name = args
            .first()
            .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
            .unwrap_or_default();
        let sub = |i: usize| {
            args.get(i)
                .map(|a| String::from_utf8_lossy(a).to_ascii_lowercase())
                .unwrap_or_default()
        };
        match name.as_str() {
            "SCAN" => return self.cluster_scan(c, &args).await,
            "DBSIZE" => {
                let mut total = 0i64;
                for mut conn in self.primary_conns().await? {
                    total += redis::cmd("DBSIZE")
                        .query_async::<i64>(&mut conn)
                        .await
                        .map_err(map_err)?;
                }
                return Ok(V::Int(total));
            }
            "INFO" if sub(1) == "keyspace" => {
                let mut replies = Vec::new();
                for mut conn in self.primary_conns().await? {
                    replies.push(
                        redis::cmd("INFO")
                            .arg("keyspace")
                            .query_async::<String>(&mut conn)
                            .await
                            .map_err(map_err)?,
                    );
                }
                return Ok(V::BulkString(
                    cluster::merge_keyspace(&replies).into_bytes(),
                ));
            }
            "CONFIG" if sub(1) == "get" && sub(2) == "databases" => {
                return Ok(V::Array(vec![
                    V::BulkString(b"databases".to_vec()),
                    V::BulkString(b"1".to_vec()),
                ]));
            }
            "SELECT" if sub(1) != "0" => {
                return Err(Error::Unsupported(
                    "a Redis Cluster has only database 0 (SELECT is not available)".into(),
                ));
            }
            _ => {}
        }
        let mut addr = match cluster::key_index(&args) {
            Some(i) => self.node_of(c, &args[i]),
            None => c
                .ranges()
                .first()
                .map(|r| r.node.clone())
                .unwrap_or_default(),
        };
        let mut asking = false;
        let mut last = None;
        for _ in 0..5 {
            let mut conn = self.node_conn(c, &addr).await?;
            let reply = if asking {
                let mut pipe = redis::pipe();
                pipe.cmd("ASKING").ignore().add_command(cmd.clone());
                pipe.query_async::<Vec<V>>(&mut conn)
                    .await
                    .map(|mut v| v.pop().unwrap_or(V::Nil))
            } else {
                cmd.query_async::<V>(&mut conn).await
            };
            match reply {
                Ok(v) => return Ok(v),
                Err(e) => match cluster::redirect(e.code(), e.detail()) {
                    Some(Redirect::Moved(to)) => {
                        self.refresh_slots(c).await;
                        addr = to;
                        asking = false;
                    }
                    Some(Redirect::Ask(to)) => {
                        addr = to;
                        asking = true;
                    }
                    None if e.is_connection_dropped() || e.is_io_error() => {
                        c.conns.lock().await.remove(&addr);
                        self.refresh_slots(c).await;
                        last = Some(map_err(e));
                    }
                    None => return Err(map_err(e)),
                },
            }
        }
        Err(last.unwrap_or_else(|| {
            Error::Connection("too many cluster redirections for one command".into())
        }))
    }

    /// `SCAN` over every primary: the cursor carries the node index (see `cluster.rs`).
    async fn cluster_scan(&self, c: &ClusterState, args: &[Vec<u8>]) -> Result<redis::Value> {
        use redis::Value as V;
        let cursor: u64 = args
            .get(1)
            .and_then(|a| std::str::from_utf8(a).ok())
            .and_then(|a| a.parse().ok())
            .unwrap_or(0);
        let nodes = cluster::primaries(&c.ranges());
        let (index, node_cursor) = cluster::decode_cursor(cursor);
        let Some(addr) = nodes.get(index) else {
            // The cluster lost a primary since the cursor was handed out: stop here.
            return Ok(V::Array(vec![
                V::BulkString(b"0".to_vec()),
                V::Array(Vec::new()),
            ]));
        };
        let mut cmd = redis::cmd("SCAN");
        cmd.arg(node_cursor);
        for a in args.iter().skip(2) {
            cmd.arg(a.as_slice());
        }
        let mut conn = self.node_conn(c, addr).await?;
        let (next, keys): (String, V) = cmd.query_async(&mut conn).await.map_err(map_err)?;
        let next = cluster::next_cursor(index, next.parse().unwrap_or(0), nodes.len()).ok_or_else(
            || Error::Connection("SCAN cursor of a cluster node is too large".into()),
        )?;
        Ok(V::Array(vec![
            V::BulkString(next.to_string().into_bytes()),
            keys,
        ]))
    }
}

/// The arguments of `cmd` as bytes (`args[0]` is the command name).
pub fn cmd_args(cmd: &Cmd) -> Vec<Vec<u8>> {
    cmd.args_iter()
        .map(|a| match a {
            redis::Arg::Simple(b) => b.to_vec(),
            // A cursor argument (`cursor_arg`) is sent as `0` by a plain query.
            _ => b"0".to_vec(),
        })
        .collect()
}

/// Ask the sentinels (`host:port` then `options.hosts`) where the master named by
/// `options.sentinelMaster` is. Sentinels usually need no password; one that asks for it gets
/// the profile's credentials.
async fn sentinel_master(cfg: &ConnectionConfig, secret: Option<&Secret>) -> Result<(String, u16)> {
    let master = cfg
        .options
        .get(SENTINEL_MASTER)
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            Error::Invalid(format!(
                "options.{SENTINEL_MASTER}: the name of the master the sentinels watch is required"
            ))
        })?
        .to_string();
    let timeout = cfg
        .connect_timeout_ms
        .map_or(DEFAULT_CONNECT_TIMEOUT, Duration::from_millis);
    let use_tls = !matches!(cfg.tls.mode, TlsMode::Disable | TlsMode::Prefer);
    let mut last = None;
    for (host, port) in cfg.nodes() {
        let mut node = cfg.clone();
        node.host = host.clone();
        node.port = port;
        let mut anonymous = node.clone();
        anonymous.username = None;
        for (who, pass) in [(&anonymous, None), (&node, secret)] {
            let client = build_client(who, pass, use_tls, 0)?;
            let mut conn = match open_with(&client, timeout, None).await {
                Ok(c) => c,
                Err(e) => {
                    last = Some(map_err(e));
                    break;
                }
            };
            let reply: std::result::Result<Option<(String, String)>, RedisError> =
                redis::cmd("SENTINEL")
                    .arg("get-master-addr-by-name")
                    .arg(&master)
                    .query_async(&mut conn)
                    .await;
            match reply {
                Ok(Some((h, p))) => {
                    let p = p.parse::<u16>().map_err(|_| {
                        Error::Connection(format!(
                            "sentinel {host}:{port} gave an invalid port `{p}`"
                        ))
                    })?;
                    return Ok((h, p));
                }
                Ok(None) => {
                    last = Some(Error::NotFound(format!(
                        "sentinel {host}:{port} does not know a master named `{master}`"
                    )));
                    break;
                }
                // A sentinel with a password: try again with the profile's login.
                Err(e)
                    if pass.is_none() && matches!(e.code(), Some("NOAUTH") | Some("WRONGPASS")) => {
                }
                Err(e) => {
                    last = Some(map_err(e));
                    break;
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| Error::Connection("no sentinel to ask".into())))
}
