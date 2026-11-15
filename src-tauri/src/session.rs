//! Live database sessions. Pure Rust (no Tauri types) so it can be tested with mock drivers.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use dbstudio_core::{
    ConnectionConfig, DbKind, DriverDescriptor, DriverRegistry, Error, Result, ScriptDialect,
    Secret, ServerInfo, Session,
};
use dbstudio_ssh::{KnownHosts, MemoryKnownHosts, Tunnel};
use serde::Serialize;
use uuid::Uuid;

use crate::events::{NullEmitter, SESSION_STATE, SessionHealth, SessionStateEvent};
use crate::lease::LeaseManager;

/// Connect timeout used when the profile does not specify one.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 15_000;

/// Returned by `session_open`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    /// Identifier of the new session.
    pub session_id: Uuid,
    /// Profile the session was opened from.
    pub connection_id: Uuid,
    /// Server product and version.
    pub server_info: ServerInfo,
    /// Descriptor (capabilities) of the driver in use.
    pub descriptor: DriverDescriptor,
    /// Script language of the session (editor lexer, splitter, parameters, classifier).
    pub dialect: ScriptDialect,
}

/// Destinations a MongoDB SOCKS5 proxy may reach: the profile's host and every member of the
/// `hosts` seed list (`host[:port]`, `[v6]:port`, comma separated).
fn mongo_allowed_hosts(config: &ConnectionConfig) -> Vec<(String, u16)> {
    let mut out = vec![(config.host.clone(), config.port)];
    if let Some(list) = config.options.get("hosts") {
        for item in list.split(',').map(str::trim).filter(|h| !h.is_empty()) {
            let (host, port) = match item.strip_prefix('[') {
                Some(rest) => match rest.split_once(']') {
                    Some((h, tail)) => (h, tail.strip_prefix(':')),
                    None => (rest, None),
                },
                None => match item.rsplit_once(':') {
                    Some((h, p)) => (h, Some(p)),
                    None => (item, None),
                },
            };
            let port = port.and_then(|p| p.parse().ok()).unwrap_or(27017);
            let entry = (host.to_string(), port);
            if !out
                .iter()
                .any(|(h, p)| h.eq_ignore_ascii_case(&entry.0) && *p == entry.1)
            {
                out.push(entry);
            }
        }
    }
    out
}

/// What a connection with an SSH tunnel needs besides the profile: the trusted host keys and
/// the tunnel secret (password / key passphrase, keychain account `<connId>#ssh`).
#[derive(Clone)]
pub struct SshEnv {
    /// Trusted host keys (`ssh_known_hosts`).
    pub known: Arc<dyn KnownHosts>,
    /// SSH password or key passphrase.
    pub secret: Option<Secret>,
}

impl std::fmt::Debug for SshEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshEnv")
            .field("secret", &self.secret.is_some())
            .finish_non_exhaustive()
    }
}

struct Entry {
    connection_id: Uuid,
    /// Profile settings at open time (read-only / environment for the guard).
    config: ConnectionConfig,
    session: Arc<dyn Session>,
    /// SSH tunnel carrying the session; closed together with it.
    tunnel: Option<Tunnel>,
    /// The password the session was opened with, kept in memory for the life of the session so
    /// a lost connection can be reopened without asking again. Never serialized or logged.
    secret: Option<Secret>,
}

/// Run a task on the tokio runtime of the caller, or on Tauri's runtime outside of one
/// (application setup).
pub(crate) fn spawn_background<F>(task: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(task);
        }
        Err(_) => {
            tauri::async_runtime::spawn(task);
        }
    }
}

/// Authentication failure on a profile set to "no authentication": the server wants a password,
/// so say how to fix the profile instead of leaving the driver's bare message.
pub(crate) fn no_auth_hint(config: &ConnectionConfig, error: Error) -> Error {
    match error {
        Error::Authentication(m) if config.is_no_auth() => Error::Authentication(format!(
            "{m} (this connection is set to \"no authentication\" but the server requires a \
             login: edit the connection and set Authentication to \"Username and password\")"
        )),
        other => other,
    }
}

/// A boxed future (the health monitor needs a dyn-compatible async callback).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a reconnect connects with.
pub struct Resolved {
    /// The profile as it is saved now (the user may have edited host, port or TLS since the
    /// session was opened); `None` keeps the profile the session was opened with.
    pub config: Option<ConnectionConfig>,
    /// Password to use (the keychain is read again, so a changed stored password wins).
    pub secret: Option<Secret>,
    /// SSH tunnel environment.
    pub ssh: Option<SshEnv>,
}

/// Resolves what a reconnect needs besides the session: the current profile, the password
/// (stored one, else the one the user typed at open time) and the SSH environment.
pub trait Credentials: Send + Sync {
    /// Credentials for reconnecting a session of `config`; `remembered` is the password the
    /// user typed when opening, if the profile does not store one.
    fn resolve<'a>(
        &'a self,
        config: &'a ConnectionConfig,
        remembered: Option<Secret>,
    ) -> BoxFuture<'a, Result<Resolved>>;
}

/// [`Credentials`] over the application state (keychain, saved profile and SSH host keys).
pub struct StateCredentials(pub crate::state::AppState);

impl Credentials for StateCredentials {
    fn resolve<'a>(
        &'a self,
        config: &'a ConnectionConfig,
        remembered: Option<Secret>,
    ) -> BoxFuture<'a, Result<Resolved>> {
        Box::pin(async move {
            let state = &self.0;
            let svc = state.connections.clone();
            let id = config.id;
            let fallback = remembered.clone();
            let (fresh, secret) = crate::commands::blocking(move || {
                match svc.session_credentials(id, remembered) {
                    Ok((profile, secret)) => Ok((Some(profile.config), secret)),
                    // The profile was deleted meanwhile: keep going with what we have.
                    Err(Error::NotFound(_)) => Ok((None, fallback)),
                    Err(e) => Err(e),
                }
            })
            .await?;
            let ssh =
                crate::commands::ssh::ssh_env(state, fresh.as_ref().unwrap_or(config)).await?;
            Ok(Resolved {
                config: fresh,
                secret,
                ssh,
            })
        })
    }
}

/// Arguments of [`SessionManager::reconnect`].
#[derive(Default)]
pub struct Reconnect {
    /// Profile to connect with instead of the one the session was opened with.
    pub config: Option<ConnectionConfig>,
    /// Password to connect with.
    pub secret: Option<Secret>,
    /// SSH environment.
    pub ssh: Option<SshEnv>,
    /// Keep `secret` in memory for later reconnects (the user typed it; a keychain password is
    /// read again every time instead).
    pub remember: bool,
}

/// Errors that mean "the connection is gone" (as opposed to a bad statement).
///
/// A `Timeout` is not one: a busy pool or a slow server also answers late. The health monitor
/// counts consecutive timeouts instead ([`TIMEOUT_STRIKES`]).
pub(crate) fn is_connection_loss(error: &Error) -> bool {
    matches!(error, Error::Connection(_) | Error::Tls(_))
}

/// Consecutive health-check timeouts that make a session count as lost.
pub const TIMEOUT_STRIKES: u32 = 3;
/// A parked (given up) session is probed once every this many health rounds.
const PARKED_EVERY: u32 = 4;
/// Time a lease gets to answer when a reconnect decides whether to keep it.
const LEASE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Pings that take longer than this count as a lost connection.
const HEALTH_PING_TIMEOUT: Duration = Duration::from_secs(10);

/// Waits before the automatic reconnect attempts (CONN-12).
pub const RECONNECT_BACKOFF: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
];

/// What a reconnect did.
#[derive(Debug)]
pub struct Reconnected {
    /// The session, with the same id as before.
    pub info: SessionInfo,
    /// Leases reopened on the new connection and leases left for `lease_reopen`.
    pub leases: crate::lease::ReopenReport,
}

/// Removes a session id from the "reconnect running" set when dropped.
struct ReconnectGuard<'a> {
    set: &'a Mutex<HashSet<Uuid>>,
    id: Uuid,
}

impl Drop for ReconnectGuard<'_> {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

/// What `connect` produced.
struct Connected {
    session: Arc<dyn Session>,
    server_info: ServerInfo,
    descriptor: DriverDescriptor,
    dialect: ScriptDialect,
    tunnel: Option<Tunnel>,
}

/// Registry of open sessions. Several sessions may be open for the same connection.
pub struct SessionManager {
    registry: Arc<DriverRegistry>,
    leases: Arc<LeaseManager>,
    sessions: Mutex<HashMap<Uuid, Entry>>,
    /// Script dialect of the last session opened per connection (survives `close`), so an
    /// import preview without a session can still use the right dialect.
    last_dialect: Mutex<HashMap<Uuid, ScriptDialect>>,
    /// Sessions the health monitor found lost and has not recovered (reconnecting or given up).
    unhealthy: Mutex<HashSet<Uuid>>,
    /// Sessions with a reconnect in flight.
    reconnecting: Mutex<HashSet<Uuid>>,
    /// Lost sessions nobody is retrying (gave up, or auto-reconnect off) with the number of
    /// health rounds since they were parked; probed sparsely so they can come back.
    parked: Mutex<HashMap<Uuid, u32>>,
    /// Consecutive health-check timeouts per session.
    timeouts: Mutex<HashMap<Uuid, u32>>,
    /// Waits between automatic reconnect attempts.
    backoff: Vec<Duration>,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("open", &self.len())
            .finish()
    }
}

impl SessionManager {
    /// Create a manager that opens sessions through `registry` and has its own lease registry
    /// (no events are published).
    #[allow(dead_code)] // the application uses `with_leases`; tests use this one
    pub fn new(registry: Arc<DriverRegistry>) -> Self {
        Self::with_leases(registry, Arc::new(LeaseManager::new(Arc::new(NullEmitter))))
    }

    /// Create a manager that closes the leases of a session in `leases` before the session.
    pub fn with_leases(registry: Arc<DriverRegistry>, leases: Arc<LeaseManager>) -> Self {
        SessionManager {
            registry,
            leases,
            sessions: Mutex::new(HashMap::new()),
            last_dialect: Mutex::new(HashMap::new()),
            unhealthy: Mutex::new(HashSet::new()),
            reconnecting: Mutex::new(HashSet::new()),
            parked: Mutex::new(HashMap::new()),
            timeouts: Mutex::new(HashMap::new()),
            backoff: RECONNECT_BACKOFF.to_vec(),
        }
    }

    /// Replace the waits between automatic reconnect attempts (tests use tiny ones).
    #[cfg(test)]
    pub fn with_backoff(mut self, backoff: Vec<Duration>) -> Self {
        self.backoff = backoff;
        self
    }

    /// Script dialect of the last session opened for `connection_id`, if any was.
    #[allow(dead_code)] // read by the import preview
    pub fn last_dialect(&self, connection_id: Uuid) -> Option<ScriptDialect> {
        self.last_dialect
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&connection_id)
            .copied()
    }

    fn remember_dialect(&self, connection_id: Uuid, dialect: ScriptDialect) {
        self.last_dialect
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(connection_id, dialect);
    }

    fn unhealthy(&self) -> MutexGuard<'_, HashSet<Uuid>> {
        self.unhealthy.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(
        &self,
        entry: (Uuid, Uuid),
        state: SessionHealth,
        attempt: u32,
        error: Option<&Error>,
        leases: (Vec<Uuid>, Vec<Uuid>),
    ) {
        crate::events::emit_json(
            &*self.leases.emitter(),
            SESSION_STATE,
            &SessionStateEvent {
                session_id: entry.0,
                connection_id: entry.1,
                state,
                attempt,
                error: error.and_then(|e| serde_json::to_value(e).ok()),
                leases_broken: leases.0,
                leases_reopened: leases.1,
            },
        );
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Uuid, Entry>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Number of open sessions.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// True when no session is open.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Connect using the driver for `config.kind` (with timeout) and fetch the server info.
    /// The caller owns the returned session.
    async fn connect(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
        ssh: Option<&SshEnv>,
    ) -> Result<Connected> {
        let driver = self.registry.get(config.kind).ok_or_else(|| {
            Error::Unsupported(format!("driver for {}", config.kind.display_name()))
        })?;
        let mut descriptor = driver.descriptor();
        let limit = Duration::from_millis(
            config
                .connect_timeout_ms
                .filter(|ms| *ms > 0)
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS),
        );
        // SSH: the driver connects to the local end of the tunnel and verifies the server
        // certificate against the original host name.
        let (tunnelled, tunnel) = match &config.ssh {
            Some(cfg) => {
                let fallback = SshEnv {
                    known: Arc::new(MemoryKnownHosts::new()),
                    secret: None,
                };
                let env = ssh.unwrap_or(&fallback);
                if config.kind == DbKind::MongoDb {
                    // MongoDB: one SOCKS5 proxy over the SSH session. The profile keeps its
                    // original host / port (TLS verifies the real name, a replica set reaches
                    // every member); the driver is told where the proxy listens.
                    let t = Tunnel::open_socks5(
                        cfg,
                        env.secret.as_ref(),
                        mongo_allowed_hosts(config),
                        env.known.as_ref(),
                    )
                    .await?;
                    let mut c = config.clone();
                    c.options.insert(
                        "socks5Proxy".into(),
                        format!("127.0.0.1:{}", t.local_addr().port()),
                    );
                    (Some(c), Some(t))
                } else {
                    let t = Tunnel::open(
                        cfg,
                        env.secret.as_ref(),
                        (config.host.clone(), config.port),
                        env.known.as_ref(),
                    )
                    .await?;
                    let mut c = config.clone();
                    c.host = "127.0.0.1".to_string();
                    c.port = t.local_addr().port();
                    if c.tls.server_name.is_none() {
                        c.tls.server_name = Some(config.host.clone());
                    }
                    (Some(c), Some(t))
                }
            }
            None => (None, None),
        };
        let config_for_driver = tunnelled.as_ref().unwrap_or(config);
        let attempt = async {
            let session: Arc<dyn Session> = Arc::from(
                driver
                    .connect(config_for_driver, config.effective_secret(secret))
                    .await?,
            );
            match session.server_info().await {
                Ok(info) => Ok((session, info)),
                Err(e) => {
                    let _ = session.close().await;
                    Err(e)
                }
            }
        };
        let (session, info) = tokio::time::timeout(limit, attempt)
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|e| no_auth_hint(config, e))?;
        // The server may differ from the driver's static description (flavour, version gates).
        if let Some(capabilities) = session.capabilities() {
            descriptor.capabilities = capabilities;
        }
        let dialect = session.script_dialect();
        Ok(Connected {
            session,
            server_info: info,
            descriptor,
            dialect,
            tunnel,
        })
    }

    /// Open a new session for `config`.
    #[allow(dead_code)] // the application uses `open_with_ssh`; tests use this one
    pub async fn open(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<SessionInfo> {
        self.open_with_ssh(config, secret, None).await
    }

    /// Open a new session for `config`, through an SSH tunnel when the profile has one.
    pub async fn open_with_ssh(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
        ssh: Option<&SshEnv>,
    ) -> Result<SessionInfo> {
        self.open_remembering(config, secret, ssh, true).await
    }

    /// Like [`SessionManager::open_with_ssh`]; `remember` keeps the password in memory for
    /// reconnects (pass false when it came from the keychain: it is read again instead).
    pub async fn open_remembering(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
        ssh: Option<&SshEnv>,
        remember: bool,
    ) -> Result<SessionInfo> {
        let Connected {
            session,
            server_info,
            descriptor,
            dialect,
            tunnel,
        } = self.connect(config, secret, ssh).await?;
        let session_id = Uuid::new_v4();
        self.remember_dialect(config.id, dialect);
        self.lock().insert(
            session_id,
            Entry {
                connection_id: config.id,
                config: config.clone(),
                session,
                tunnel,
                secret: secret.filter(|_| remember).cloned(),
            },
        );
        tracing::info!(
            session = %session_id,
            connection = %config.id,
            kind = config.kind.id(),
            host = %config.host,
            user = config.username.as_deref().unwrap_or(""),
            "session opened"
        );
        Ok(SessionInfo {
            session_id,
            connection_id: config.id,
            server_info,
            descriptor,
            dialect,
        })
    }

    /// Connect, read the server info and disconnect again (used by "Test connection").
    #[allow(dead_code)] // the application uses `test_with_ssh`; tests use this one
    pub async fn test(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<ServerInfo> {
        self.test_with_ssh(config, secret, None).await
    }

    /// Like [`SessionManager::test`], through an SSH tunnel when the profile has one.
    pub async fn test_with_ssh(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
        ssh: Option<&SshEnv>,
    ) -> Result<ServerInfo> {
        let Connected {
            session,
            server_info: info,
            ..
        } = self.connect(config, secret, ssh).await?;
        if let Err(e) = session.close().await {
            tracing::debug!(error = %e, "closing test session failed");
        }
        tracing::info!(
            kind = config.kind.id(),
            host = %config.host,
            user = config.username.as_deref().unwrap_or(""),
            "connection test succeeded"
        );
        Ok(info)
    }

    /// The open session, or `NotFound`.
    pub fn get(&self, session_id: Uuid) -> Result<Arc<dyn Session>> {
        self.lock()
            .get(&session_id)
            .map(|e| Arc::clone(&e.session))
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))
    }

    /// The open session together with the profile settings it was opened with.
    pub fn get_with_config(
        &self,
        session_id: Uuid,
    ) -> Result<(Arc<dyn Session>, ConnectionConfig)> {
        self.lock()
            .get(&session_id)
            .map(|e| (Arc::clone(&e.session), e.config.clone()))
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))
    }

    /// Any open session opened from `connection_id` with its id (which one is unspecified when
    /// several are open), or `None` when the connection has no open session.
    #[allow(dead_code)] // used by the export/backup commands
    pub fn find_by_connection(&self, connection_id: Uuid) -> Option<(Uuid, Arc<dyn Session>)> {
        self.lock()
            .iter()
            .find(|(_, e)| e.connection_id == connection_id)
            .map(|(id, e)| (*id, Arc::clone(&e.session)))
    }

    /// The password the session was opened (or last reconnected) with, if it was typed.
    pub fn remembered_secret(&self, session_id: Uuid) -> Option<Secret> {
        self.lock().get(&session_id).and_then(|e| e.secret.clone())
    }

    /// Open a new connection for an existing session and swap it in; the session id stays.
    /// Leases of the session: those that still answer a ping are left alone, idle
    /// auto-commit ones are reopened on the new connection (same ids), the others (a
    /// transaction in flight, auto-commit off) are marked lost and wait for `lease_reopen`.
    pub async fn reconnect(&self, session_id: Uuid, args: Reconnect) -> Result<Reconnected> {
        let (current, connection_id) = {
            let map = self.lock();
            let entry = map
                .get(&session_id)
                .ok_or_else(|| Error::NotFound(format!("session {session_id}")))?;
            (entry.config.clone(), entry.connection_id)
        };
        let config = args.config.unwrap_or(current);
        if !self
            .reconnecting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id)
        {
            return Err(Error::Busy("a reconnect of this session is running".into()));
        }
        let _guard = ReconnectGuard {
            set: &self.reconnecting,
            id: session_id,
        };
        let secret = args.secret;
        let Connected {
            session,
            server_info,
            descriptor,
            dialect,
            tunnel,
        } = self
            .connect(&config, secret.as_ref(), args.ssh.as_ref())
            .await?;
        // The session may have been closed while we were connecting.
        let old = {
            let mut map = self.lock();
            match map.get_mut(&session_id) {
                Some(entry) => {
                    let old_session = std::mem::replace(&mut entry.session, Arc::clone(&session));
                    let old_tunnel = std::mem::replace(&mut entry.tunnel, tunnel);
                    entry.config = config.clone();
                    if args.remember && secret.is_some() {
                        entry.secret = secret.clone();
                    }
                    Some((old_session, old_tunnel))
                }
                None => None,
            }
        };
        let Some((old_session, old_tunnel)) = old else {
            let _ = session.close().await;
            return Err(Error::NotFound(format!("session {session_id}")));
        };
        self.remember_dialect(connection_id, dialect);
        // The dead connection: close it without waiting long, then drop its tunnel.
        let _ = tokio::time::timeout(Duration::from_secs(3), old_session.close()).await;
        drop(old_tunnel);
        let leases = self.recover_leases(session_id, &session).await;
        self.unhealthy().remove(&session_id);
        self.parked_map().remove(&session_id);
        self.timeouts_map().remove(&session_id);
        tracing::info!(session = %session_id, connection = %connection_id, "session reconnected");
        Ok(Reconnected {
            info: SessionInfo {
                session_id,
                connection_id,
                server_info,
                descriptor,
                dialect,
            },
            leases,
        })
    }

    fn parked_map(&self) -> MutexGuard<'_, HashMap<Uuid, u32>> {
        self.parked.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn timeouts_map(&self) -> MutexGuard<'_, HashMap<Uuid, u32>> {
        self.timeouts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Decide what happens to each lease of `session_id` after its session got a new
    /// connection: keep the ones that still answer, reopen idle auto-commit ones on `session`
    /// (their `SET`s, temp tables and `search_path` are gone: `reopened` lets the UI say so),
    /// mark the rest lost.
    async fn recover_leases(
        &self,
        session_id: Uuid,
        session: &Arc<dyn Session>,
    ) -> crate::lease::ReopenReport {
        let mut report = crate::lease::ReopenReport::default();
        let surveys = self.leases.survey_session(session_id);
        // Which leases are still alive? Ask them all at once.
        let mut probes = tokio::task::JoinSet::new();
        for survey in &surveys {
            let leases = Arc::clone(&self.leases);
            let id = survey.lease_id;
            probes.spawn(async move { (id, leases.probe(id, LEASE_PROBE_TIMEOUT).await) });
        }
        let mut alive = HashSet::new();
        while let Some(done) = probes.join_next().await {
            if let Ok((id, Some(true))) = done {
                alive.insert(id);
            }
        }
        for survey in surveys {
            if alive.contains(&survey.lease_id) {
                continue;
            }
            let reopenable = survey.state.auto_commit
                && survey.state.transaction == dbstudio_core::TxStatus::Idle;
            if reopenable {
                let options = dbstudio_core::LeaseOptions {
                    database: survey.state.database.clone(),
                    schema: survey.state.schema.clone(),
                    auto_commit: true,
                };
                match session.open_lease(options).await {
                    Ok(lease) => match self.leases.replace(survey.lease_id, lease).await {
                        Ok(_) => {
                            report.reopened.push(survey.lease_id);
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(lease = %survey.lease_id, error = %e, "cannot swap in the reopened lease")
                        }
                    },
                    Err(e) => {
                        tracing::warn!(lease = %survey.lease_id, error = %e, "cannot reopen lease")
                    }
                }
            }
            self.leases.mark_lost(survey.lease_id);
            report.broken.push(survey.lease_id);
        }
        report.reopened.sort();
        report.broken.sort();
        report
    }

    /// Manual reconnect (the user asked): same as [`SessionManager::reconnect`] and publishes
    /// `Connected` with the leases that need attention.
    pub async fn reconnect_manual(&self, session_id: Uuid, args: Reconnect) -> Result<Reconnected> {
        let done = self.reconnect(session_id, args).await?;
        self.publish(
            (session_id, done.info.connection_id),
            SessionHealth::Connected,
            0,
            None,
            (done.leases.broken.clone(), done.leases.reopened.clone()),
        );
        Ok(done)
    }

    /// One round of the health monitor: ping every session that is not being recovered; for
    /// each lost one publish `Lost` and, with `auto`, start the reconnect task. Parked
    /// sessions (gave up, or `auto` off) are probed once every few rounds and announced
    /// `Connected` when they answer again. Returns the sessions found lost.
    pub async fn health_pass(
        self: &Arc<Self>,
        creds: &Arc<dyn Credentials>,
        auto: bool,
    ) -> Vec<Uuid> {
        let targets: Vec<(Uuid, Uuid, Arc<dyn Session>, bool)> = {
            let unhealthy = self.unhealthy();
            let mut parked = self.parked_map();
            self.lock()
                .iter()
                .filter_map(|(id, e)| {
                    let is_parked = parked.contains_key(id);
                    if unhealthy.contains(id) {
                        if !is_parked {
                            // A reconnect task is working on it.
                            return None;
                        }
                        let rounds = parked.entry(*id).or_insert(0);
                        *rounds += 1;
                        if *rounds % PARKED_EVERY != 0 {
                            return None;
                        }
                    }
                    Some((*id, e.connection_id, Arc::clone(&e.session), is_parked))
                })
                .collect()
        };
        let mut pings = tokio::task::JoinSet::new();
        for (id, connection_id, session, was_parked) in targets {
            pings.spawn(async move {
                let result = match tokio::time::timeout(HEALTH_PING_TIMEOUT, session.ping()).await {
                    Ok(r) => r,
                    Err(_) => Err(Error::Timeout),
                };
                (id, connection_id, session, was_parked, result)
            });
        }
        let mut lost = Vec::new();
        while let Some(done) = pings.join_next().await {
            let Ok((id, connection_id, pinged, was_parked, result)) = done else {
                continue;
            };
            // The session may have been reconnected (or replaced) while we pinged it: an
            // error of the old connection says nothing about the current one.
            let current = self
                .lock()
                .get(&id)
                .is_some_and(|e| Arc::ptr_eq(&e.session, &pinged));
            if !current {
                continue;
            }
            let error = match result {
                Ok(()) => {
                    self.timeouts_map().remove(&id);
                    if was_parked {
                        self.unhealthy().remove(&id);
                        self.parked_map().remove(&id);
                        tracing::info!(session = %id, "parked session answers again");
                        self.publish(
                            (id, connection_id),
                            SessionHealth::Connected,
                            0,
                            None,
                            (vec![], vec![]),
                        );
                    }
                    continue;
                }
                Err(e) => e,
            };
            if was_parked {
                continue;
            }
            if matches!(error, Error::Timeout) {
                // A slow answer is not a dead connection: only repeated ones are.
                let strikes = {
                    let mut map = self.timeouts_map();
                    let n = map.entry(id).or_insert(0);
                    *n += 1;
                    *n
                };
                if strikes < TIMEOUT_STRIKES {
                    continue;
                }
            } else if !is_connection_loss(&error) {
                continue;
            }
            self.timeouts_map().remove(&id);
            lost.push(id);
            let manager = Arc::clone(self);
            let creds = Arc::clone(creds);
            spawn_background(async move {
                manager
                    .handle_lost(id, connection_id, error, creds, auto)
                    .await;
            });
        }
        lost
    }

    /// React to a lost session: publish `Lost`, then (when `auto`) retry with backoff. Every
    /// attempt first checks that the session is still lost: when somebody (the user, by hand)
    /// got it back, this task has nothing left to do.
    async fn handle_lost(
        self: &Arc<Self>,
        session_id: Uuid,
        connection_id: Uuid,
        error: Error,
        creds: Arc<dyn Credentials>,
        auto: bool,
    ) {
        if !self.unhealthy().insert(session_id) {
            return;
        }
        let ids = (session_id, connection_id);
        tracing::warn!(session = %session_id, error = %error, "session lost");
        self.publish(ids, SessionHealth::Lost, 0, Some(&error), (vec![], vec![]));
        if !auto {
            self.parked_map().insert(session_id, 0);
            return;
        }
        let mut last = error;
        for (attempt, wait) in self.backoff.iter().enumerate() {
            tokio::time::sleep(*wait).await;
            if !self.unhealthy().contains(&session_id) {
                // Recovered by hand (or closed) while we waited.
                return;
            }
            let attempt = u32::try_from(attempt + 1).unwrap_or(u32::MAX);
            let Some((config, remembered)) = self
                .lock()
                .get(&session_id)
                .map(|e| (e.config.clone(), e.secret.clone()))
            else {
                // Closed by the user meanwhile.
                self.unhealthy().remove(&session_id);
                return;
            };
            self.publish(
                ids,
                SessionHealth::Reconnecting,
                attempt,
                None,
                (vec![], vec![]),
            );
            let outcome = match creds.resolve(&config, remembered).await {
                Ok(resolved) => {
                    self.reconnect(
                        session_id,
                        Reconnect {
                            config: resolved.config,
                            secret: resolved.secret,
                            ssh: resolved.ssh,
                            remember: false,
                        },
                    )
                    .await
                }
                Err(e) => Err(e),
            };
            match outcome {
                Ok(done) => {
                    self.publish(
                        ids,
                        SessionHealth::Reconnected,
                        attempt,
                        None,
                        (done.leases.broken, done.leases.reopened),
                    );
                    return;
                }
                // Somebody else is reconnecting it right now: look again after the next wait.
                Err(Error::Busy(_)) => continue,
                Err(e) => {
                    tracing::info!(session = %session_id, attempt, error = %e, "reconnect failed");
                    // A refused password or an unknown SSH host key will not fix itself.
                    let hopeless = matches!(
                        e,
                        Error::Authentication(_) | Error::SshHostKey { .. } | Error::NotFound(_)
                    );
                    last = e;
                    if hopeless {
                        break;
                    }
                }
            }
        }
        if !self.unhealthy().contains(&session_id) {
            return;
        }
        self.parked_map().insert(session_id, 0);
        self.publish(
            ids,
            SessionHealth::GaveUp,
            u32::try_from(self.backoff.len()).unwrap_or(u32::MAX),
            Some(&last),
            (vec![], vec![]),
        );
    }

    /// Start the background health monitor: every `keepalive_secs` ping each session, publish
    /// `session://state` and reconnect when `auto_reconnect` is on (CONN-12); also keeps the
    /// leases alive. Reads the settings before every round.
    pub fn spawn_health(self: &Arc<Self>, state: &crate::state::AppState) {
        let creds: Arc<dyn Credentials> = Arc::new(StateCredentials(state.clone()));
        let svc = state.connections.clone();
        let keepalive_svc = svc.clone();
        self.leases.spawn_keepalive(move || {
            keepalive_svc
                .settings()
                .ok()
                .filter(|s| s.keepalive_secs > 0)
                .map(|s| Duration::from_secs(u64::from(s.keepalive_secs)))
        });
        let manager = Arc::downgrade(self);
        spawn_background(async move {
            loop {
                let settings = svc.settings().ok();
                let secs = settings.as_ref().map_or(0, |s| s.keepalive_secs);
                if secs == 0 {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    if manager.strong_count() == 0 {
                        return;
                    }
                    continue;
                }
                tokio::time::sleep(Duration::from_secs(u64::from(secs))).await;
                let Some(manager) = manager.upgrade() else {
                    return;
                };
                let auto = svc.settings().map_or(true, |s| s.auto_reconnect);
                manager.health_pass(&creds, auto).await;
            }
        });
    }

    /// Close a session (its leases first). Closing an unknown (already closed) session is not an error.
    pub async fn close(&self, session_id: Uuid) -> Result<()> {
        let entry = self.lock().remove(&session_id);
        self.unhealthy().remove(&session_id);
        self.parked_map().remove(&session_id);
        self.timeouts_map().remove(&session_id);
        match entry {
            Some(entry) => {
                self.leases.close_all_for_session(session_id).await;
                Self::shut_down(session_id, entry).await
            }
            None => Ok(()),
        }
    }

    /// Close every session opened from `connection_id`; returns how many were closed.
    pub async fn close_connection(&self, connection_id: Uuid) -> usize {
        let doomed: Vec<(Uuid, Entry)> = {
            let mut map = self.lock();
            let ids: Vec<Uuid> = map
                .iter()
                .filter(|(_, e)| e.connection_id == connection_id)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| map.remove(&id).map(|e| (id, e)))
                .collect()
        };
        let count = doomed.len();
        for (id, entry) in doomed {
            self.leases.close_all_for_session(id).await;
            let _ = Self::shut_down(id, entry).await;
        }
        count
    }

    /// Close every open session (application exit).
    pub async fn close_all(&self) {
        let all: Vec<(Uuid, Entry)> = self.lock().drain().collect();
        for (id, entry) in all {
            self.leases.close_all_for_session(id).await;
            let _ = Self::shut_down(id, entry).await;
        }
    }

    async fn shut_down(id: Uuid, entry: Entry) -> Result<()> {
        let result = entry.session.close().await;
        match &result {
            Ok(()) => {
                tracing::info!(session = %id, connection = %entry.connection_id, "session closed")
            }
            Err(e) => tracing::warn!(session = %id, error = %e, "session close failed"),
        }
        result
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Mock driver and session shared by the unit tests of this crate.

    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use dbstudio_core::{
        Capabilities, ColumnMeta, DbKind, Driver, EndOutcome, ExecSummary, ObjectNode, ObjectPath,
        QueryRequest, ResultSink, StatementEnd, TableCountRequest, TableDataRequest, Value,
        ValueKind,
    };

    use super::*;

    /// Decrements `counter` when it is positive; true when it did (a pending failure to inject).
    fn take_one(counter: &AtomicUsize) -> bool {
        let mut n = counter.load(Ordering::SeqCst);
        while n > 0 {
            match counter.compare_exchange_weak(n, n - 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return true,
                Err(current) => n = current,
            }
        }
        false
    }

    /// What the mock driver observed.
    #[derive(Debug, Default)]
    pub struct Probe {
        pub connects: AtomicUsize,
        pub closes: AtomicUsize,
        pub last_secret: Mutex<Option<Option<String>>>,
        pub cancels: Mutex<Vec<Uuid>>,
        /// Texts passed to `Session::execute`.
        pub executed: Mutex<Vec<String>>,
        /// `(max_rows, timeout_ms)` of the last `Session::execute`.
        pub last_limits: Mutex<(Option<u64>, Option<u64>)>,
        /// Every connection (session or lease) opened up to this birth number is dead: its
        /// `ping` fails with a connection error.
        pub killed_through: AtomicUsize,
        /// The next this-many session pings answer `Timeout`.
        pub ping_timeouts: AtomicUsize,
        /// The next this-many `connect` calls fail with a connection error.
        pub connect_failures: AtomicUsize,
        /// Leases opened on mock sessions.
        pub lease_opens: AtomicUsize,
        /// When set, the mock sessions open leases and describe tables (off: the driver
        /// defaults, `Unsupported`).
        pub rich: std::sync::atomic::AtomicBool,
        /// Number of connections (sessions and leases) ever opened.
        pub births: AtomicUsize,
        /// Dialect the mock sessions report (default: opaque).
        pub dialect: Mutex<Option<ScriptDialect>>,
    }

    impl Probe {
        /// Kill every connection opened so far; later ones are healthy.
        pub fn kill_all(&self) {
            self.killed_through
                .store(self.births.load(Ordering::SeqCst), Ordering::SeqCst);
        }
    }

    /// Driver whose sessions stream three row batches; it fails with `Authentication` when
    /// `require_password` is set and no secret is supplied.
    #[derive(Debug)]
    pub struct MockDriver {
        pub kind: DbKind,
        pub probe: Arc<Probe>,
        pub require_password: bool,
        pub connect_delay: Option<Duration>,
    }

    impl MockDriver {
        pub fn new(kind: DbKind) -> (Arc<MockDriver>, Arc<Probe>) {
            let probe = Arc::new(Probe::default());
            (
                Arc::new(MockDriver {
                    kind,
                    probe: probe.clone(),
                    require_password: false,
                    connect_delay: None,
                }),
                probe,
            )
        }
    }

    #[derive(Debug)]
    pub struct MockSession {
        probe: Arc<Probe>,
        birth: usize,
    }

    /// A lease of a mock session; it dies with the connection generation it was born in.
    #[derive(Debug)]
    pub struct MockLease {
        probe: Arc<Probe>,
        birth: usize,
        state: Mutex<dbstudio_core::LeaseState>,
    }

    impl MockLease {
        fn dead(&self) -> bool {
            self.birth <= self.probe.killed_through.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Driver for MockDriver {
        fn descriptor(&self) -> DriverDescriptor {
            DriverDescriptor::new(
                self.kind,
                Capabilities::planned_for(self.kind),
                dbstudio_core::DriverStatus::Available,
            )
        }

        async fn connect(
            &self,
            _config: &ConnectionConfig,
            secret: Option<&Secret>,
        ) -> Result<Box<dyn Session>> {
            if let Some(d) = self.connect_delay {
                tokio::time::sleep(d).await;
            }
            *self.probe.last_secret.lock().unwrap() = Some(secret.map(|s| s.expose().to_string()));
            if self.require_password && secret.is_none() {
                return Err(Error::Authentication("password required".into()));
            }
            if take_one(&self.probe.connect_failures) {
                return Err(Error::Connection("connection refused".into()));
            }
            self.probe.connects.fetch_add(1, Ordering::SeqCst);
            let birth = self.probe.births.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Box::new(MockSession {
                probe: self.probe.clone(),
                birth,
            }))
        }
    }

    #[async_trait]
    impl Session for MockSession {
        async fn ping(&self) -> Result<()> {
            if take_one(&self.probe.ping_timeouts) {
                return Err(Error::Timeout);
            }
            if self.birth <= self.probe.killed_through.load(Ordering::SeqCst) {
                return Err(Error::Connection("server closed the connection".into()));
            }
            Ok(())
        }
        fn script_dialect(&self) -> ScriptDialect {
            self.probe
                .dialect
                .lock()
                .unwrap()
                .unwrap_or(ScriptDialect::Opaque)
        }
        async fn open_lease(
            &self,
            opts: dbstudio_core::LeaseOptions,
        ) -> Result<Box<dyn dbstudio_core::Lease>> {
            if !self.probe.rich.load(Ordering::SeqCst) {
                return Err(Error::Unsupported("leases".into()));
            }
            self.probe.lease_opens.fetch_add(1, Ordering::SeqCst);
            // A lease is a connection of its own, born after the session.
            let birth = self.probe.births.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Box::new(MockLease {
                probe: self.probe.clone(),
                birth: birth.max(self.birth),
                state: Mutex::new(dbstudio_core::LeaseState::initial(&opts)),
            }))
        }
        async fn server_info(&self) -> Result<ServerInfo> {
            Ok(ServerInfo {
                product: "MockDB".into(),
                version: "1.2.3".into(),
                ..Default::default()
            })
        }
        async fn execute(
            &self,
            req: QueryRequest,
            sink: &mut dyn ResultSink,
        ) -> Result<ExecSummary> {
            self.probe.executed.lock().unwrap().push(req.text.clone());
            *self.probe.last_limits.lock().unwrap() = (req.max_rows, req.timeout_ms);
            if req.text == "fail" {
                return Err(Error::Query {
                    message: "boom".into(),
                    position: Some(2),
                });
            }
            let len = u32::try_from(req.text.chars().count()).unwrap_or(u32::MAX);
            sink.on_statement_start(dbstudio_core::StatementStart {
                index: 0,
                range: dbstudio_core::TextRange { start: 0, end: len },
            })?;
            sink.on_columns(vec![ColumnMeta {
                name: "n".into(),
                type_name: "int8".into(),
                value_kind: ValueKind::Int,
                nullable: Some(false),
            }])?;
            sink.on_rows(vec![vec![Value::Int(1)]])?;
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Rows {
                    row_count: 1,
                    truncated: false,
                },
                elapsed_ms: 1,
            })?;
            Ok(ExecSummary {
                statements: 1,
                elapsed_ms: 1,
            })
        }
        async fn cancel(&self, query_id: Uuid) -> Result<()> {
            self.probe.cancels.lock().unwrap().push(query_id);
            Ok(())
        }
        async fn list_children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
            let _ = parent;
            Ok(vec![])
        }
        async fn object_ddl(&self, _path: &ObjectPath) -> Result<String> {
            Ok("CREATE TABLE t ();".into())
        }
        async fn fetch_table(
            &self,
            req: TableDataRequest,
            sink: &mut dyn ResultSink,
        ) -> Result<ExecSummary> {
            if req.raw_where.as_deref() == Some("slow") {
                // A query that never answers until it is cancelled or dropped.
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            sink.on_columns(vec![ColumnMeta {
                name: "id".into(),
                type_name: "int8".into(),
                value_kind: ValueKind::Int,
                nullable: Some(false),
            }])?;
            for chunk in [[1i64, 2], [3, 4], [5, 6]] {
                sink.on_rows(chunk.iter().map(|i| vec![Value::Int(*i)]).collect())?;
            }
            sink.on_message("done".into())?;
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Rows {
                    row_count: req.limit.min(6),
                    truncated: false,
                },
                elapsed_ms: 1,
            })?;
            Ok(ExecSummary {
                statements: 1,
                elapsed_ms: 1,
            })
        }
        async fn count_table(&self, _req: TableCountRequest) -> Result<u64> {
            Ok(42)
        }
        async fn explain(
            &self,
            req: dbstudio_core::ExplainRequest,
        ) -> Result<dbstudio_core::RawPlan> {
            Ok(dbstudio_core::RawPlan {
                format: dbstudio_core::PlanFormat::PostgresJson,
                text: include_str!(
                    "../../crates/dbstudio-plan/tests/fixtures/pg_04_sort_spill_analyze.json"
                )
                .into(),
                statement: req.text,
                analyzed: req.analyze,
                rolled_back: req.analyze,
                server_version: "16.2".into(),
                captured_at: "2026-11-14T00:00:00Z".into(),
                extra: Default::default(),
            })
        }
        async fn table_details(&self, path: &ObjectPath) -> Result<dbstudio_core::TableDetails> {
            if !self.probe.rich.load(Ordering::SeqCst) {
                return Err(Error::Unsupported("table_details".into()));
            }
            let column =
                |name: &str, ordinal: u32, primary_key: bool| dbstudio_core::ColumnDetail {
                    name: name.into(),
                    ordinal,
                    type_name: "int8".into(),
                    value_kind: ValueKind::Int,
                    nullable: !primary_key,
                    default: None,
                    comment: None,
                    primary_key,
                    extra: None,
                };
            Ok(dbstudio_core::TableDetails {
                path: path.clone(),
                kind: dbstudio_core::ObjectKind::Table,
                comment: None,
                columns: vec![column("id", 1, true), column("qty", 2, false)],
                indexes: vec![],
                foreign_keys: vec![],
                estimated_rows: None,
                size_bytes: None,
            })
        }
        async fn close(&self) -> Result<()> {
            self.probe.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[async_trait]
    impl dbstudio_core::Lease for MockLease {
        async fn execute(&self, _: QueryRequest, _: &mut dyn ResultSink) -> Result<ExecSummary> {
            Err(Error::Unsupported("execute".into()))
        }
        async fn fetch_more(
            &self,
            _: Uuid,
            _: u64,
            _: Option<u32>,
            _: &mut dyn ResultSink,
        ) -> Result<ExecSummary> {
            Err(Error::NotFound("pending result".into()))
        }
        async fn discard_result(&self) -> Result<()> {
            Ok(())
        }
        async fn cancel(&self, _: Uuid) -> Result<()> {
            Ok(())
        }
        async fn begin(&self, _: dbstudio_core::TxOptions) -> Result<()> {
            self.state.lock().unwrap().transaction = dbstudio_core::TxStatus::Active;
            Ok(())
        }
        async fn commit(&self) -> Result<()> {
            self.state.lock().unwrap().transaction = dbstudio_core::TxStatus::Idle;
            Ok(())
        }
        async fn rollback(&self) -> Result<()> {
            self.state.lock().unwrap().transaction = dbstudio_core::TxStatus::Idle;
            Ok(())
        }
        async fn set_auto_commit(&self, on: bool) -> Result<()> {
            self.state.lock().unwrap().auto_commit = on;
            Ok(())
        }
        async fn set_context(&self, ctx: dbstudio_core::LeaseContext) -> Result<()> {
            let mut st = self.state.lock().unwrap();
            if ctx.database.is_some() {
                st.database = ctx.database;
            }
            if ctx.schema.is_some() {
                st.schema = ctx.schema;
            }
            Ok(())
        }
        fn state(&self) -> dbstudio_core::LeaseState {
            self.state.lock().unwrap().clone()
        }
        async fn ping(&self) -> Result<()> {
            if self.dead() {
                return Err(Error::Connection("server closed the connection".into()));
            }
            Ok(())
        }
        async fn close(&self) -> Result<dbstudio_core::LeaseCloseReport> {
            self.probe.closes.fetch_add(1, Ordering::SeqCst);
            Ok(dbstudio_core::LeaseCloseReport {
                rolled_back: false,
                forced: false,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use dbstudio_core::DbKind;

    use super::testing::*;
    use super::*;

    fn config(kind: DbKind) -> ConnectionConfig {
        ConnectionConfig::new(kind, "t", "localhost")
    }

    #[test]
    fn mongo_proxy_allows_the_profile_and_every_seed_host() {
        let mut c = ConnectionConfig::new(DbKind::MongoDb, "m", "Mongo7");
        c.port = 27017;
        assert_eq!(mongo_allowed_hosts(&c), vec![("Mongo7".to_string(), 27017)]);
        c.options.insert(
            "hosts".into(),
            "mongo7:27017, a.example:27018,[::1]:27019,bare".into(),
        );
        assert_eq!(
            mongo_allowed_hosts(&c),
            vec![
                ("Mongo7".to_string(), 27017),
                ("a.example".to_string(), 27018),
                ("::1".to_string(), 27019),
                ("bare".to_string(), 27017),
            ]
        );
    }

    fn manager(driver: Arc<MockDriver>) -> SessionManager {
        let mut registry = DriverRegistry::new();
        registry.register(driver);
        SessionManager::new(Arc::new(registry))
    }

    #[tokio::test]
    async fn no_auth_profile_never_passes_a_secret_and_hints_on_auth_failure() {
        let (driver, probe) = MockDriver::new(DbKind::Redis);
        let mgr = manager(driver);
        let mut cfg = config(DbKind::Redis);
        cfg.auth_mode = dbstudio_core::AuthMode::NoAuth;
        mgr.open(&cfg, Some(&Secret::new("stale"))).await.unwrap();
        assert_eq!(*probe.last_secret.lock().unwrap(), Some(None));

        let probe = Arc::new(Probe::default());
        let strict = Arc::new(MockDriver {
            kind: DbKind::Redis,
            probe,
            require_password: true,
            connect_delay: None,
        });
        let mgr = manager(strict);
        let err = mgr.open(&cfg, Some(&Secret::new("x"))).await.unwrap_err();
        assert_eq!(err.kind(), "authentication");
        assert!(err.to_string().contains("no authentication"), "{err}");
        // The password mode keeps the driver's bare message.
        cfg.auth_mode = dbstudio_core::AuthMode::Password;
        let err = mgr.open(&cfg, None).await.unwrap_err();
        assert!(!err.to_string().contains("no authentication"), "{err}");
    }

    #[tokio::test]
    async fn open_get_close() {
        let (driver, probe) = MockDriver::new(DbKind::Postgres);
        let mgr = manager(driver);
        let cfg = config(DbKind::Postgres);
        let info = mgr.open(&cfg, None).await.unwrap();
        assert_eq!(info.connection_id, cfg.id);
        assert_eq!(info.server_info.product, "MockDB");
        assert_eq!(info.descriptor.kind, DbKind::Postgres);
        // The mock session keeps the default dialect.
        assert_eq!(info.dialect, dbstudio_core::ScriptDialect::Opaque);
        assert_eq!(
            serde_json::to_value(&info).unwrap()["dialect"],
            serde_json::json!({"name": "opaque"})
        );
        assert_eq!(mgr.len(), 1);
        assert!(mgr.get(info.session_id).is_ok());

        mgr.close(info.session_id).await.unwrap();
        assert!(mgr.is_empty());
        assert_eq!(probe.closes.load(Ordering::SeqCst), 1);
        assert!(matches!(mgr.get(info.session_id), Err(Error::NotFound(_))));
        // Closing again is harmless.
        mgr.close(info.session_id).await.unwrap();
        assert_eq!(probe.closes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn config_is_kept_per_session() {
        let (driver, _) = MockDriver::new(DbKind::Postgres);
        let mgr = manager(driver);
        let mut cfg = config(DbKind::Postgres);
        cfg.read_only = true;
        let info = mgr.open(&cfg, None).await.unwrap();
        let (_, kept) = mgr.get_with_config(info.session_id).unwrap();
        assert!(kept.read_only);
        assert_eq!(kept.id, cfg.id);
        assert!(matches!(
            mgr.get_with_config(Uuid::new_v4()),
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn closing_a_session_closes_its_leases_first() {
        use crate::lease::testing::FakeLease;
        let (driver, _) = MockDriver::new(DbKind::Postgres);
        let mut registry = DriverRegistry::new();
        registry.register(driver);
        let leases = Arc::new(LeaseManager::new(Arc::new(NullEmitter)));
        let mgr = SessionManager::with_leases(Arc::new(registry), leases.clone());
        let cfg = config(DbKind::Postgres);
        let a = mgr.open(&cfg, None).await.unwrap();
        let b = mgr.open(&cfg, None).await.unwrap();
        let closes = Arc::new(AtomicUsize::new(0));
        let mk = || -> Box<dyn dbstudio_core::Lease> {
            Box::new(FakeLease {
                closes: closes.clone(),
            })
        };
        leases.register(a.session_id, mk()).unwrap();
        leases.register(a.session_id, mk()).unwrap();
        let other = leases.register(b.session_id, mk()).unwrap();

        mgr.close(a.session_id).await.unwrap();
        assert_eq!(closes.load(Ordering::SeqCst), 2);
        assert_eq!(leases.count_for_session(a.session_id), 0);
        assert!(leases.get(other.lease_id).is_ok());

        // close_connection and close_all do the same.
        assert_eq!(mgr.close_connection(cfg.id).await, 1);
        assert_eq!(closes.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn find_by_connection_returns_a_session_of_that_connection() {
        let (driver, _) = MockDriver::new(DbKind::Postgres);
        let mgr = manager(driver);
        let a = config(DbKind::Postgres);
        let b = config(DbKind::Postgres);
        assert!(mgr.find_by_connection(a.id).is_none());
        let info_a = mgr.open(&a, None).await.unwrap();
        let info_b = mgr.open(&b, None).await.unwrap();
        assert_eq!(mgr.find_by_connection(a.id).unwrap().0, info_a.session_id);
        assert_eq!(mgr.find_by_connection(b.id).unwrap().0, info_b.session_id);
        mgr.close(info_a.session_id).await.unwrap();
        assert!(mgr.find_by_connection(a.id).is_none());
        assert!(mgr.find_by_connection(Uuid::new_v4()).is_none());
    }

    #[tokio::test]
    async fn unknown_session_is_not_found() {
        let (driver, _) = MockDriver::new(DbKind::Postgres);
        let mgr = manager(driver);
        assert!(matches!(mgr.get(Uuid::new_v4()), Err(Error::NotFound(_))));
    }

    #[tokio::test]
    async fn unregistered_driver_is_unsupported() {
        let (driver, probe) = MockDriver::new(DbKind::Postgres);
        let mgr = manager(driver);
        let err = mgr.open(&config(DbKind::Redis), None).await.unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)));
        assert!(mgr.test(&config(DbKind::Redis), None).await.is_err());
        assert_eq!(probe.connects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn several_sessions_per_connection_and_close_connection() {
        let (driver, probe) = MockDriver::new(DbKind::MySql);
        let mgr = manager(driver);
        let a = config(DbKind::MySql);
        let b = config(DbKind::MySql);
        let s1 = mgr.open(&a, None).await.unwrap();
        let s2 = mgr.open(&a, None).await.unwrap();
        let s3 = mgr.open(&b, None).await.unwrap();
        assert_ne!(s1.session_id, s2.session_id);
        assert_eq!(mgr.len(), 3);

        assert_eq!(mgr.close_connection(a.id).await, 2);
        assert!(mgr.get(s1.session_id).is_err() && mgr.get(s2.session_id).is_err());
        assert!(mgr.get(s3.session_id).is_ok());
        assert_eq!(probe.closes.load(Ordering::SeqCst), 2);

        mgr.close_all().await;
        assert!(mgr.is_empty());
        assert_eq!(probe.closes.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn connect_times_out() {
        let (mut driver, _) = MockDriver::new(DbKind::Postgres);
        Arc::get_mut(&mut driver).unwrap().connect_delay = Some(Duration::from_secs(30));
        let mgr = manager(driver);
        let mut cfg = config(DbKind::Postgres);
        cfg.connect_timeout_ms = Some(30);
        assert!(matches!(mgr.open(&cfg, None).await, Err(Error::Timeout)));
        assert!(mgr.is_empty());
    }

    #[tokio::test]
    async fn test_connects_and_closes_without_registering() {
        let (driver, probe) = MockDriver::new(DbKind::Postgres);
        let mgr = manager(driver);
        let info = mgr
            .test(&config(DbKind::Postgres), Some(&Secret::new("pw")))
            .await
            .unwrap();
        assert_eq!(info.version, "1.2.3");
        assert!(mgr.is_empty());
        assert_eq!(probe.closes.load(Ordering::SeqCst), 1);
        assert_eq!(
            probe.last_secret.lock().unwrap().clone(),
            Some(Some("pw".to_string()))
        );
    }

    #[tokio::test]
    async fn unreachable_bastion_fails_before_the_driver_is_used() {
        use dbstudio_core::{SshAuth, SshConfig};
        // Port that nobody listens on.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let (driver, probe) = MockDriver::new(DbKind::Postgres);
        let m = manager(driver);
        let mut cfg = config(DbKind::Postgres);
        cfg.ssh = Some(SshConfig {
            host: "127.0.0.1".into(),
            port,
            username: "u".into(),
            auth: SshAuth::Password,
            keepalive_secs: None,
            // Windows retries SYN on a closed loopback port for ~2 s before refusing: leave room
            // for the refusal so the error stays `Connection`, not `Timeout`.
            connect_timeout_ms: Some(10_000),
        });
        let err = m.open(&cfg, None).await.unwrap_err();
        assert!(matches!(err, Error::Connection(_)), "{err:?}");
        assert!(m.is_empty());
        assert_eq!(probe.closes.load(std::sync::atomic::Ordering::SeqCst), 0);
        let err = m.test(&cfg, None).await.unwrap_err();
        assert!(matches!(err, Error::Connection(_)), "{err:?}");
    }

    // ---- health, reconnect, last dialect ----

    use crate::events::EventHub;

    struct NoCreds;
    impl Credentials for NoCreds {
        fn resolve<'a>(
            &'a self,
            _: &'a ConnectionConfig,
            remembered: Option<Secret>,
        ) -> BoxFuture<'a, Result<Resolved>> {
            Box::pin(async move {
                Ok(Resolved {
                    config: None,
                    secret: remembered,
                    ssh: None,
                })
            })
        }
    }

    struct Rig {
        mgr: Arc<SessionManager>,
        leases: Arc<LeaseManager>,
        hub: Arc<EventHub>,
        probe: Arc<Probe>,
        cfg: ConnectionConfig,
    }

    fn rig(backoff_ms: &[u64]) -> Rig {
        let (driver, probe) = MockDriver::new(DbKind::Postgres);
        probe.rich.store(true, Ordering::SeqCst);
        let mut registry = DriverRegistry::new();
        registry.register(driver);
        let hub = EventHub::new();
        let leases = Arc::new(LeaseManager::new(hub.clone()));
        let mgr = Arc::new(
            SessionManager::with_leases(Arc::new(registry), leases.clone()).with_backoff(
                backoff_ms
                    .iter()
                    .map(|ms| Duration::from_millis(*ms))
                    .collect(),
            ),
        );
        Rig {
            mgr,
            leases,
            hub,
            probe,
            cfg: config(DbKind::Postgres),
        }
    }

    /// `(state, attempt, leases_broken)` of the next `session://state` event.
    async fn next_state(
        rx: &mut tokio::sync::broadcast::Receiver<crate::events::AppEvent>,
    ) -> (String, u64, Vec<String>) {
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("no session event")
                .unwrap();
            if ev.name == "session://state" {
                return (
                    ev.payload["state"].as_str().unwrap().to_string(),
                    ev.payload["attempt"].as_u64().unwrap(),
                    ev.payload["leasesBroken"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_string())
                        .collect(),
                );
            }
        }
    }

    #[tokio::test]
    async fn last_dialect_survives_closing_the_session() {
        let r = rig(&[]);
        assert_eq!(r.mgr.last_dialect(r.cfg.id), None);
        *r.probe.dialect.lock().unwrap() = Some(ScriptDialect::MySql {
            mariadb: true,
            backslash_escapes: true,
            ansi_quotes: false,
        });
        let info = r.mgr.open(&r.cfg, None).await.unwrap();
        r.mgr.close(info.session_id).await.unwrap();
        assert!(matches!(
            r.mgr.last_dialect(r.cfg.id),
            Some(ScriptDialect::MySql { mariadb: true, .. })
        ));
    }

    #[tokio::test]
    async fn reconnect_keeps_the_id_reopens_idle_leases_and_flags_open_transactions() {
        let r = rig(&[]);
        let info = r
            .mgr
            .open(&r.cfg, Some(&Secret::new("typed-pw")))
            .await
            .unwrap();
        let session = r.mgr.get(info.session_id).unwrap();
        let idle = r
            .leases
            .register(
                info.session_id,
                session
                    .open_lease(dbstudio_core::LeaseOptions {
                        database: Some("app".into()),
                        schema: Some("s1".into()),
                        auto_commit: true,
                    })
                    .await
                    .unwrap(),
            )
            .unwrap();
        let in_tx = r
            .leases
            .register(
                info.session_id,
                session
                    .open_lease(dbstudio_core::LeaseOptions::default())
                    .await
                    .unwrap(),
            )
            .unwrap();
        r.leases
            .get(in_tx.lease_id)
            .unwrap()
            .begin(dbstudio_core::TxOptions::default())
            .await
            .unwrap();
        r.probe.kill_all();
        assert!(session.ping().await.is_err());

        let remembered = r.mgr.remembered_secret(info.session_id);
        let done = r
            .mgr
            .reconnect(
                info.session_id,
                Reconnect {
                    secret: remembered,
                    remember: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(done.info.session_id, info.session_id);
        assert_eq!(r.mgr.len(), 1);
        assert!(r.mgr.get(info.session_id).unwrap().ping().await.is_ok());
        assert_eq!(done.leases.reopened, vec![idle.lease_id]);
        assert_eq!(done.leases.broken, vec![in_tx.lease_id]);
        // The idle lease lives on the new connection with its context.
        let st = r.leases.state_of(idle.lease_id).unwrap();
        assert!(!st.broken);
        assert_eq!(st.schema.as_deref(), Some("s1"));
        r.leases.get(idle.lease_id).unwrap().ping().await.unwrap();
        // The lease with a transaction is not reopened behind the user's back.
        assert!(r.leases.state_of(in_tx.lease_id).unwrap().broken);
        assert!(r.leases.needs_reopen(in_tx.lease_id).unwrap());
        assert_eq!(r.probe.lease_opens.load(Ordering::SeqCst), 3);
        // ...until it is reopened explicitly: the transaction is reported lost.
        let (session_id, options) = r.leases.reopen_options(in_tx.lease_id).unwrap();
        assert_eq!(session_id, info.session_id);
        let fresh = r
            .mgr
            .get(session_id)
            .unwrap()
            .open_lease(options)
            .await
            .unwrap();
        assert!(r.leases.replace(in_tx.lease_id, fresh).await.unwrap());
        assert!(!r.leases.state_of(in_tx.lease_id).unwrap().broken);
        // The password typed at open time is kept for later reconnects, in memory only.
        assert_eq!(
            r.mgr.remembered_secret(info.session_id).unwrap().expose(),
            "typed-pw"
        );
        assert_eq!(
            r.probe.last_secret.lock().unwrap().clone(),
            Some(Some("typed-pw".to_string()))
        );
        assert!(matches!(
            r.mgr.reconnect(Uuid::new_v4(), Reconnect::default()).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn a_lost_session_reconnects_by_itself_with_backoff() {
        let r = rig(&[1, 1, 1]);
        let mut rx = r.hub.subscribe();
        let info = r.mgr.open(&r.cfg, None).await.unwrap();
        let creds: Arc<dyn Credentials> = Arc::new(NoCreds);
        // Healthy sessions are left alone.
        assert!(r.mgr.health_pass(&creds, true).await.is_empty());

        r.probe.kill_all();
        r.probe.connect_failures.store(1, Ordering::SeqCst);
        assert_eq!(r.mgr.health_pass(&creds, true).await, vec![info.session_id]);
        let seen = [
            next_state(&mut rx).await,
            next_state(&mut rx).await,
            next_state(&mut rx).await,
            next_state(&mut rx).await,
        ];
        let names: Vec<_> = seen.iter().map(|(s, a, _)| (s.as_str(), *a)).collect();
        assert_eq!(
            names,
            [
                ("lost", 0),
                ("reconnecting", 1),
                ("reconnecting", 2),
                ("reconnected", 2)
            ]
        );
        // Same id, working again, and no longer "lost": the next round finds nothing.
        assert!(r.mgr.get(info.session_id).unwrap().ping().await.is_ok());
        assert!(r.mgr.health_pass(&creds, true).await.is_empty());
        assert_eq!(r.probe.connects.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn reconnect_gives_up_after_the_last_attempt_and_manual_reconnect_recovers() {
        let r = rig(&[1, 1]);
        let mut rx = r.hub.subscribe();
        let info = r.mgr.open(&r.cfg, None).await.unwrap();
        let creds: Arc<dyn Credentials> = Arc::new(NoCreds);
        r.probe.kill_all();
        r.probe.connect_failures.store(100, Ordering::SeqCst);
        r.mgr.health_pass(&creds, true).await;
        let mut last = ("".to_string(), 0, vec![]);
        for _ in 0..4 {
            last = next_state(&mut rx).await;
        }
        assert_eq!((last.0.as_str(), last.1), ("gaveUp", 2));
        // The session stays registered (its tabs keep their state) but is not probed again.
        assert_eq!(r.mgr.len(), 1);
        assert!(r.mgr.health_pass(&creds, true).await.is_empty());
        // The user retries by hand once the server is back.
        r.probe.connect_failures.store(0, Ordering::SeqCst);
        let done = r
            .mgr
            .reconnect_manual(
                info.session_id,
                Reconnect {
                    secret: Some(Secret::new("new-pw")),
                    remember: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(done.info.session_id, info.session_id);
        assert_eq!(next_state(&mut rx).await.0, "connected");
        assert_eq!(
            r.mgr.remembered_secret(info.session_id).unwrap().expose(),
            "new-pw"
        );
        // Probed again afterwards.
        r.probe.kill_all();
        assert_eq!(
            r.mgr.health_pass(&creds, false).await,
            vec![info.session_id]
        );
        // Without auto-reconnect only `lost` is published.
        assert_eq!(next_state(&mut rx).await.0, "lost");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_manual_reconnect_stops_the_automatic_one_and_spares_the_new_transaction() {
        let r = rig(&[80, 80, 80]);
        let mut rx = r.hub.subscribe();
        let info = r.mgr.open(&r.cfg, None).await.unwrap();
        let creds: Arc<dyn Credentials> = Arc::new(NoCreds);
        r.probe.kill_all();
        assert_eq!(r.mgr.health_pass(&creds, true).await, vec![info.session_id]);
        assert_eq!(next_state(&mut rx).await.0, "lost");
        // The user reconnects by hand before the first automatic attempt...
        r.mgr
            .reconnect_manual(info.session_id, Reconnect::default())
            .await
            .unwrap();
        assert_eq!(next_state(&mut rx).await.0, "connected");
        // ...and starts a transaction on a lease of the healthy session.
        let session = r.mgr.get(info.session_id).unwrap();
        let lease = r
            .leases
            .register(
                info.session_id,
                session
                    .open_lease(dbstudio_core::LeaseOptions::default())
                    .await
                    .unwrap(),
            )
            .unwrap();
        r.leases
            .get(lease.lease_id)
            .unwrap()
            .begin(dbstudio_core::TxOptions::default())
            .await
            .unwrap();
        // The backoff task wakes up, sees the session is fine and does nothing.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(rx.try_recv().is_err(), "no further session event");
        assert!(!r.leases.state_of(lease.lease_id).unwrap().broken);
        assert_eq!(
            r.leases.state_of(lease.lease_id).unwrap().transaction,
            dbstudio_core::TxStatus::Active
        );
        assert_eq!(r.probe.connects.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_late_ping_is_only_a_loss_after_three_strikes() {
        let r = rig(&[1]);
        let info = r.mgr.open(&r.cfg, None).await.unwrap();
        let creds: Arc<dyn Credentials> = Arc::new(NoCreds);
        r.probe.ping_timeouts.store(2, Ordering::SeqCst);
        assert!(r.mgr.health_pass(&creds, false).await.is_empty());
        assert!(r.mgr.health_pass(&creds, false).await.is_empty());
        // An answer in between resets the count.
        assert!(r.mgr.health_pass(&creds, false).await.is_empty());
        r.probe.ping_timeouts.store(3, Ordering::SeqCst);
        for _ in 0..2 {
            assert!(r.mgr.health_pass(&creds, false).await.is_empty());
        }
        assert_eq!(
            r.mgr.health_pass(&creds, false).await,
            vec![info.session_id]
        );
    }

    #[tokio::test]
    async fn a_reconnect_keeps_leases_that_still_answer_and_reports_what_it_reopened() {
        let r = rig(&[]);
        let mut rx = r.hub.subscribe();
        let info = r.mgr.open(&r.cfg, None).await.unwrap();
        let session = r.mgr.get(info.session_id).unwrap();
        let open = |auto_commit: bool| {
            let session = session.clone();
            async move {
                session
                    .open_lease(dbstudio_core::LeaseOptions {
                        database: None,
                        schema: None,
                        auto_commit,
                    })
                    .await
                    .unwrap()
            }
        };
        let survivor = r
            .leases
            .register(info.session_id, open(true).await)
            .unwrap();
        // Everything opened so far dies, then one new lease is born healthy.
        r.probe.kill_all();
        let fresh = r
            .leases
            .register(info.session_id, open(true).await)
            .unwrap();
        let manual = r
            .leases
            .register(info.session_id, open(false).await)
            .unwrap();
        // `manual` is born after the kill as well (healthy), but is not auto-commit; kill only
        // the first lease by marking it lost the way a failed keepalive would.
        r.leases.mark_lost(survivor.lease_id);
        let opens_before = r.probe.lease_opens.load(Ordering::SeqCst);
        let done = r
            .mgr
            .reconnect_manual(info.session_id, Reconnect::default())
            .await
            .unwrap();
        // The lost auto-commit lease is reopened; the healthy ones are left alone.
        assert_eq!(done.leases.reopened, vec![survivor.lease_id]);
        assert!(done.leases.broken.is_empty());
        assert_eq!(r.probe.lease_opens.load(Ordering::SeqCst), opens_before + 1);
        assert!(!r.leases.state_of(fresh.lease_id).unwrap().broken);
        assert!(!r.leases.state_of(manual.lease_id).unwrap().broken);
        let mut payload = None;
        while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            if ev.name == "session://state" {
                payload = Some(ev.payload);
            }
        }
        let payload = payload.expect("a session event");
        assert_eq!(
            payload["leasesReopened"],
            serde_json::json!([survivor.lease_id.to_string()])
        );

        // A lost lease without auto-commit is never reopened behind the user's back.
        r.leases.mark_lost(manual.lease_id);
        let done = r
            .mgr
            .reconnect_manual(info.session_id, Reconnect::default())
            .await
            .unwrap();
        assert_eq!(done.leases.broken, vec![manual.lease_id]);
        assert!(r.leases.needs_reopen(manual.lease_id).unwrap());
    }

    #[tokio::test]
    async fn parked_sessions_are_probed_now_and_then_and_come_back_by_themselves() {
        let r = rig(&[1]);
        let mut rx = r.hub.subscribe();
        let info = r.mgr.open(&r.cfg, None).await.unwrap();
        let creds: Arc<dyn Credentials> = Arc::new(NoCreds);
        r.probe.kill_all();
        r.probe.connect_failures.store(100, Ordering::SeqCst);
        r.mgr.health_pass(&creds, true).await;
        let mut states = Vec::new();
        for _ in 0..3 {
            states.push(next_state(&mut rx).await.0);
        }
        assert_eq!(states, ["lost", "reconnecting", "gaveUp"]);
        // The server comes back on its own: the old connection answers again.
        r.probe.killed_through.store(0, Ordering::SeqCst);
        for round in 1..PARKED_EVERY {
            assert!(
                r.mgr.health_pass(&creds, true).await.is_empty(),
                "round {round}: parked sessions are only probed every {PARKED_EVERY}th round"
            );
            assert!(rx.try_recv().is_err());
        }
        r.mgr.health_pass(&creds, true).await;
        assert_eq!(next_state(&mut rx).await.0, "connected");
        // Healthy again: pinged every round, no more parking.
        r.probe.kill_all();
        assert_eq!(
            r.mgr.health_pass(&creds, false).await,
            vec![info.session_id]
        );
    }

    #[tokio::test]
    async fn auto_reconnect_off_still_parks_and_recovers() {
        let r = rig(&[1]);
        let mut rx = r.hub.subscribe();
        r.mgr.open(&r.cfg, None).await.unwrap();
        let creds: Arc<dyn Credentials> = Arc::new(NoCreds);
        r.probe.kill_all();
        r.mgr.health_pass(&creds, false).await;
        assert_eq!(next_state(&mut rx).await.0, "lost");
        r.probe.killed_through.store(0, Ordering::SeqCst);
        for _ in 0..PARKED_EVERY {
            r.mgr.health_pass(&creds, false).await;
        }
        assert_eq!(next_state(&mut rx).await.0, "connected");
    }

    #[tokio::test]
    async fn only_typed_passwords_stay_in_memory() {
        let r = rig(&[]);
        let cfg = &r.cfg;
        let typed = r
            .mgr
            .open_remembering(cfg, Some(&Secret::new("typed")), None, true)
            .await
            .unwrap();
        let stored = r
            .mgr
            .open_remembering(cfg, Some(&Secret::new("from-keychain")), None, false)
            .await
            .unwrap();
        assert_eq!(
            r.mgr.remembered_secret(typed.session_id).unwrap().expose(),
            "typed"
        );
        assert!(r.mgr.remembered_secret(stored.session_id).is_none());
        // A reconnect with a keychain password does not start remembering it.
        r.mgr
            .reconnect(
                stored.session_id,
                Reconnect {
                    secret: Some(Secret::new("from-keychain")),
                    remember: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(r.mgr.remembered_secret(stored.session_id).is_none());
    }

    #[tokio::test]
    async fn reconnect_connects_with_the_current_profile() {
        let r = rig(&[]);
        let info = r.mgr.open(&r.cfg, None).await.unwrap();
        let mut edited = r.cfg.clone();
        edited.host = "db2.internal".into();
        r.mgr
            .reconnect(
                info.session_id,
                Reconnect {
                    config: Some(edited),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let (_, kept) = r.mgr.get_with_config(info.session_id).unwrap();
        assert_eq!(kept.host, "db2.internal");
    }

    #[tokio::test]
    async fn statement_errors_are_not_a_lost_connection() {
        assert!(is_connection_loss(&Error::Connection("x".into())));
        // A late answer alone is not a lost connection.
        assert!(!is_connection_loss(&Error::Timeout));
        assert!(is_connection_loss(&Error::Tls("x".into())));
        assert!(!is_connection_loss(&Error::Busy("x".into())));
        assert!(!is_connection_loss(&Error::Authentication("x".into())));
    }
}

#[cfg(all(test, any(feature = "postgres", feature = "mysql")))]
#[path = "session_it.rs"]
mod it;
