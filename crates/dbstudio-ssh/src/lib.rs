//! SSH tunnel (local port forward) used when a connection has `ssh` configured.
//!
//! [`Tunnel::open`] connects to the bastion, verifies its host key through [`KnownHosts`]
//! (unknown or changed keys fail with `Error::SshHostKey`, before any credential is sent),
//! authenticates (password, private key with optional passphrase, or the SSH agent) and listens
//! on `127.0.0.1:0`. Every accepted connection is forwarded through a `direct-tcpip` channel to
//! the target. Dropping the [`Tunnel`] closes the listener, the forwarded connections and the
//! SSH session.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dbstudio_core::{Error, Result, Secret, SshAuth, SshConfig};
use russh::client::{self, AuthResult, Handle};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};

/// Connect timeout used when the profile does not specify one.
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 15_000;

/// What the known-hosts table says about a host key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// This exact key is trusted.
    Known,
    /// Nothing is stored for this host.
    Unknown,
    /// A different key is stored for this host.
    Changed,
}

/// Trusted SSH host keys (implemented by the store).
pub trait KnownHosts: Send + Sync {
    /// Compare `fingerprint` (`SHA256:...`) of a key of type `key_type` (`ssh-ed25519`...) with
    /// the stored one for `host:port`.
    fn check(&self, host: &str, port: u16, key_type: &str, fingerprint: &str) -> HostKeyStatus;
}

/// In-memory [`KnownHosts`], handy in tests.
#[derive(Debug, Default)]
pub struct MemoryKnownHosts {
    keys: Mutex<BTreeMap<(String, u16), (String, String)>>,
}

impl MemoryKnownHosts {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Trust `fingerprint` for `host:port` (replacing any previous key).
    pub fn trust(&self, host: &str, port: u16, key_type: &str, fingerprint: &str) {
        self.keys.lock().expect("known hosts lock").insert(
            (host.to_string(), port),
            (key_type.to_string(), fingerprint.to_string()),
        );
    }
}

impl KnownHosts for MemoryKnownHosts {
    fn check(&self, host: &str, port: u16, key_type: &str, fingerprint: &str) -> HostKeyStatus {
        match self
            .keys
            .lock()
            .expect("known hosts lock")
            .get(&(host.to_string(), port))
        {
            None => HostKeyStatus::Unknown,
            Some((t, f)) if t == key_type && f == fingerprint => HostKeyStatus::Known,
            Some(_) => HostKeyStatus::Changed,
        }
    }
}

/// Key seen during the handshake.
#[derive(Debug)]
struct Observed {
    key_type: String,
    fingerprint: String,
}

/// Client handler: reports the server key to [`Tunnel::open`] and waits for its verdict, so the
/// handshake only completes for a trusted key.
struct ClientHandler {
    observed: Option<oneshot::Sender<Observed>>,
    verdict: Option<oneshot::Receiver<bool>>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let pk = key.public_key();
        let observed = Observed {
            key_type: pk.algorithm().to_string(),
            fingerprint: pk.fingerprint(HashAlg::Sha256).to_string(),
        };
        let (Some(tx), Some(rx)) = (self.observed.take(), self.verdict.take()) else {
            return Ok(false);
        };
        if tx.send(observed).is_err() {
            return Ok(false);
        }
        Ok(rx.await.unwrap_or(false))
    }
}

/// An open tunnel. Dropping it closes the listener, the forwarded connections and the SSH
/// session.
#[derive(Debug)]
pub struct Tunnel {
    local_addr: SocketAddr,
    task: JoinHandle<()>,
}

impl Tunnel {
    /// Connect to the bastion in `cfg`, authenticate (`secret` = password or key passphrase),
    /// verify the host key through `known`, and listen on `127.0.0.1:0` forwarding every
    /// accepted connection to `target`.
    pub async fn open(
        cfg: &SshConfig,
        secret: Option<&Secret>,
        target: (String, u16),
        known: &dyn KnownHosts,
    ) -> Result<Tunnel> {
        let limit = Duration::from_millis(
            cfg.connect_timeout_ms
                .filter(|ms| *ms > 0)
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS),
        );
        let handle = tokio::time::timeout(limit, establish(cfg, secret, known))
            .await
            .map_err(|_| Error::Timeout)??;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| Error::Connection(format!("ssh tunnel: cannot listen locally: {e}")))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| Error::Connection(format!("ssh tunnel: {e}")))?;
        tracing::info!(
            bastion = %cfg.host,
            port = cfg.port,
            target_host = %target.0,
            target_port = target.1,
            local = %local_addr,
            "ssh tunnel open"
        );
        let task = tokio::spawn(accept_loop(listener, Arc::new(handle), target));
        Ok(Tunnel { local_addr, task })
    }

    /// Like [`Tunnel::open`], but the local listener speaks SOCKS5 (127.0.0.1, no authentication,
    /// `CONNECT` only) and forwards each request through a `direct-tcpip` channel. Only the
    /// destinations in `allowed` (`host`, `port`; host compared case-insensitively) are served,
    /// anything else gets "connection not allowed". A client keeps the original host name, so
    /// TLS checks the certificate against it (R6), and several hosts (a replica set) can share
    /// one SSH session.
    pub async fn open_socks5(
        cfg: &SshConfig,
        secret: Option<&Secret>,
        allowed: Vec<(String, u16)>,
        known: &dyn KnownHosts,
    ) -> Result<Tunnel> {
        let limit = Duration::from_millis(
            cfg.connect_timeout_ms
                .filter(|ms| *ms > 0)
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS),
        );
        let handle = tokio::time::timeout(limit, establish(cfg, secret, known))
            .await
            .map_err(|_| Error::Timeout)??;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| Error::Connection(format!("ssh tunnel: cannot listen locally: {e}")))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| Error::Connection(format!("ssh tunnel: {e}")))?;
        tracing::info!(
            bastion = %cfg.host,
            port = cfg.port,
            targets = allowed.len(),
            local = %local_addr,
            "ssh socks5 proxy open"
        );
        let task = tokio::spawn(socks_accept_loop(listener, Arc::new(handle), allowed));
        Ok(Tunnel { local_addr, task })
    }

    /// Local address connections are forwarded from.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn accept_loop(
    listener: TcpListener,
    handle: Arc<Handle<ClientHandler>>,
    target: (String, u16),
) {
    // Dropping the set (when this task is aborted) aborts the forwarding tasks, which drops the
    // last references to the SSH handle and ends the session.
    let mut set: JoinSet<()> = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut sock, peer) = match accepted {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::warn!(error = %e, "ssh tunnel accept failed");
                        break;
                    }
                };
                let handle = Arc::clone(&handle);
                let (host, port) = target.clone();
                set.spawn(async move {
                    let channel = match handle
                        .channel_open_direct_tcpip(host, u32::from(port), peer.ip().to_string(), u32::from(peer.port()))
                        .await
                    {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(error = %e, "ssh tunnel: cannot open forwarding channel");
                            return;
                        }
                    };
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
                });
            }
            Some(_) = set.join_next(), if !set.is_empty() => {}
        }
    }
}

async fn socks_accept_loop(
    listener: TcpListener,
    handle: Arc<Handle<ClientHandler>>,
    allowed: Vec<(String, u16)>,
) {
    let allowed = Arc::new(allowed);
    let mut set: JoinSet<()> = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (sock, peer) = match accepted {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::warn!(error = %e, "ssh socks5 accept failed");
                        break;
                    }
                };
                let handle = Arc::clone(&handle);
                let allowed = Arc::clone(&allowed);
                set.spawn(async move {
                    if let Err(e) = serve_socks5(sock, peer, &handle, &allowed).await {
                        tracing::debug!(error = %e, "ssh socks5 request failed");
                    }
                });
            }
            Some(_) = set.join_next(), if !set.is_empty() => {}
        }
    }
}

/// SOCKS5 reply code: succeeded / not allowed / host unreachable / command not supported /
/// address type not supported.
const REP_OK: u8 = 0;
const REP_NOT_ALLOWED: u8 = 2;
const REP_HOST_UNREACHABLE: u8 = 4;
const REP_CMD_UNSUPPORTED: u8 = 7;
const REP_ATYP_UNSUPPORTED: u8 = 8;

async fn socks_reply(sock: &mut TcpStream, code: u8) -> std::io::Result<()> {
    // Bound address 0.0.0.0:0.
    sock.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await
}

/// Handshake of one SOCKS5 client, then pipe it through a `direct-tcpip` channel.
async fn serve_socks5(
    mut sock: TcpStream,
    peer: SocketAddr,
    handle: &Handle<ClientHandler>,
    allowed: &[(String, u16)],
) -> std::io::Result<()> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
    // Greeting: VER NMETHODS METHODS...; we only offer "no authentication" (0).
    let mut head = [0u8; 2];
    sock.read_exact(&mut head).await?;
    if head[0] != 5 {
        return Err(bad("not a SOCKS5 client"));
    }
    let mut methods = vec![0u8; usize::from(head[1])];
    sock.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        sock.write_all(&[5, 0xff]).await?;
        return Err(bad("no acceptable authentication method"));
    }
    sock.write_all(&[5, 0]).await?;
    // Request: VER CMD RSV ATYP ADDR PORT.
    let mut req = [0u8; 4];
    sock.read_exact(&mut req).await?;
    if req[0] != 5 {
        return Err(bad("bad request version"));
    }
    let host = match req[3] {
        1 => {
            let mut a = [0u8; 4];
            sock.read_exact(&mut a).await?;
            std::net::Ipv4Addr::from(a).to_string()
        }
        3 => {
            let mut len = [0u8; 1];
            sock.read_exact(&mut len).await?;
            let mut name = vec![0u8; usize::from(len[0])];
            sock.read_exact(&mut name).await?;
            String::from_utf8(name).map_err(|_| bad("host name is not UTF-8"))?
        }
        4 => {
            let mut a = [0u8; 16];
            sock.read_exact(&mut a).await?;
            std::net::Ipv6Addr::from(a).to_string()
        }
        _ => {
            socks_reply(&mut sock, REP_ATYP_UNSUPPORTED).await?;
            return Err(bad("unsupported address type"));
        }
    };
    let mut port = [0u8; 2];
    sock.read_exact(&mut port).await?;
    let port = u16::from_be_bytes(port);
    if req[1] != 1 {
        socks_reply(&mut sock, REP_CMD_UNSUPPORTED).await?;
        return Err(bad("only CONNECT is supported"));
    }
    if !allowed
        .iter()
        .any(|(h, p)| *p == port && h.eq_ignore_ascii_case(&host))
    {
        socks_reply(&mut sock, REP_NOT_ALLOWED).await?;
        return Err(bad("destination is not allowed"));
    }
    let channel = match handle
        .channel_open_direct_tcpip(
            host,
            u32::from(port),
            peer.ip().to_string(),
            u32::from(peer.port()),
        )
        .await
    {
        Ok(c) => c,
        Err(e) => {
            socks_reply(&mut sock, REP_HOST_UNREACHABLE).await?;
            return Err(std::io::Error::other(e.to_string()));
        }
    };
    socks_reply(&mut sock, REP_OK).await?;
    let mut stream = channel.into_stream();
    tokio::io::copy_bidirectional(&mut sock, &mut stream).await?;
    Ok(())
}

fn conn_err(cfg: &SshConfig, what: &str, e: impl std::fmt::Display) -> Error {
    Error::Connection(format!("ssh {}:{}: {what}: {e}", cfg.host, cfg.port))
}

/// TCP connect, key exchange with host key verification, authentication.
async fn establish(
    cfg: &SshConfig,
    secret: Option<&Secret>,
    known: &dyn KnownHosts,
) -> Result<Handle<ClientHandler>> {
    let config = client::Config {
        keepalive_interval: cfg
            .keepalive_secs
            .filter(|s| *s > 0)
            .map(|s| Duration::from_secs(u64::from(s))),
        inactivity_timeout: None,
        ..Default::default()
    };
    let config = Arc::new(config);

    let (obs_tx, obs_rx) = oneshot::channel();
    let (verdict_tx, verdict_rx) = oneshot::channel();
    let handler = ClientHandler {
        observed: Some(obs_tx),
        verdict: Some(verdict_rx),
    };
    let connect = client::connect(config, (cfg.host.as_str(), cfg.port), handler);
    tokio::pin!(connect);
    let mut obs_rx = obs_rx;
    let mut verdict_tx = Some(verdict_tx);
    let mut rejected: Option<Error> = None;
    let result = loop {
        tokio::select! {
            r = &mut connect => break r,
            seen = &mut obs_rx, if verdict_tx.is_some() => {
                let Ok(seen) = seen else { continue };
                let status = known.check(&cfg.host, cfg.port, &seen.key_type, &seen.fingerprint);
                let trusted = status == HostKeyStatus::Known;
                if !trusted {
                    rejected = Some(Error::SshHostKey {
                        host: cfg.host.clone(),
                        port: cfg.port,
                        key_type: seen.key_type,
                        fingerprint: seen.fingerprint,
                        changed: status == HostKeyStatus::Changed,
                    });
                }
                if let Some(tx) = verdict_tx.take() {
                    let _ = tx.send(trusted);
                }
            }
        }
    };
    if let Some(e) = rejected {
        return Err(e);
    }
    let mut handle = result.map_err(|e| conn_err(cfg, "connection failed", e))?;
    authenticate(cfg, secret, &mut handle).await?;
    Ok(handle)
}

fn auth_failed(cfg: &SshConfig, how: &str) -> Error {
    Error::Authentication(format!(
        "ssh {}@{}:{}: {how} was rejected",
        cfg.username, cfg.host, cfg.port
    ))
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

async fn authenticate(
    cfg: &SshConfig,
    secret: Option<&Secret>,
    handle: &mut Handle<ClientHandler>,
) -> Result<()> {
    match &cfg.auth {
        SshAuth::Password => {
            let Some(secret) = secret else {
                return Err(Error::Authentication(
                    "ssh password authentication needs a password".into(),
                ));
            };
            let r = handle
                .authenticate_password(cfg.username.clone(), secret.expose().to_string())
                .await
                .map_err(|e| conn_err(cfg, "authentication failed", e))?;
            match r {
                AuthResult::Success => Ok(()),
                AuthResult::Failure { .. } => Err(auth_failed(cfg, "the password")),
            }
        }
        SshAuth::PrivateKey {
            key_path,
            has_passphrase,
        } => {
            let path = expand_home(key_path);
            let passphrase = if *has_passphrase {
                match secret {
                    Some(s) => Some(s.expose().to_string()),
                    None => {
                        return Err(Error::Authentication(
                            "the ssh private key needs its passphrase".into(),
                        ));
                    }
                }
            } else {
                None
            };
            let shown = path.display().to_string();
            let key = tokio::task::spawn_blocking(move || {
                load_secret_key(&path, passphrase.as_deref())
            })
            .await
            .map_err(|e| Error::Other(format!("ssh key loading failed: {e}")))?
            .map_err(|e| match e {
                russh::keys::Error::KeyIsEncrypted => Error::Authentication(format!(
                    "the ssh private key {shown} is encrypted: a passphrase is required"
                )),
                russh::keys::Error::IO(io) => {
                    Error::Authentication(format!("cannot read the ssh private key {shown}: {io}"))
                }
                other => Error::Authentication(format!(
                    "cannot use the ssh private key {shown} (wrong passphrase or unsupported format): {other}"
                )),
            })?;
            let hash = handle
                .best_supported_rsa_hash()
                .await
                .map_err(|e| conn_err(cfg, "authentication failed", e))?
                .flatten();
            let r = handle
                .authenticate_publickey(
                    cfg.username.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await
                .map_err(|e| conn_err(cfg, "authentication failed", e))?;
            match r {
                AuthResult::Success => Ok(()),
                AuthResult::Failure { .. } => Err(auth_failed(cfg, "the private key")),
            }
        }
        SshAuth::Agent => authenticate_agent(cfg, handle).await,
    }
}

#[cfg(unix)]
async fn agent_client() -> Result<russh::keys::agent::client::AgentClient<tokio::net::UnixStream>> {
    russh::keys::agent::client::AgentClient::connect_env()
        .await
        .map_err(|e| Error::Authentication(format!("cannot reach the ssh agent: {e}")))
}

#[cfg(windows)]
async fn agent_client()
-> Result<russh::keys::agent::client::AgentClient<tokio::net::windows::named_pipe::NamedPipeClient>>
{
    russh::keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
        .await
        .map_err(|e| Error::Authentication(format!("cannot reach the ssh agent: {e}")))
}

#[cfg(any(unix, windows))]
async fn authenticate_agent(cfg: &SshConfig, handle: &mut Handle<ClientHandler>) -> Result<()> {
    use russh::keys::agent::AgentIdentity;
    let mut agent = agent_client().await?;
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| Error::Authentication(format!("ssh agent: {e}")))?;
    if identities.is_empty() {
        return Err(Error::Authentication(
            "the ssh agent holds no identities".into(),
        ));
    }
    let hash = handle
        .best_supported_rsa_hash()
        .await
        .map_err(|e| conn_err(cfg, "authentication failed", e))?
        .flatten();
    for id in identities {
        let AgentIdentity::PublicKey { key, .. } = id else {
            continue;
        };
        let r = handle
            .authenticate_publickey_with(cfg.username.clone(), key, hash, &mut agent)
            .await
            .map_err(|e| Error::Authentication(format!("ssh agent: {e}")))?;
        if matches!(r, AuthResult::Success) {
            return Ok(());
        }
    }
    Err(auth_failed(cfg, "every key of the ssh agent"))
}

#[cfg(not(any(unix, windows)))]
async fn authenticate_agent(_cfg: &SshConfig, _handle: &mut Handle<ClientHandler>) -> Result<()> {
    Err(Error::Unsupported("ssh agent".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_known_hosts_statuses() {
        let k = MemoryKnownHosts::new();
        assert_eq!(
            k.check("h", 22, "ssh-ed25519", "SHA256:a"),
            HostKeyStatus::Unknown
        );
        k.trust("h", 22, "ssh-ed25519", "SHA256:a");
        assert_eq!(
            k.check("h", 22, "ssh-ed25519", "SHA256:a"),
            HostKeyStatus::Known
        );
        assert_eq!(
            k.check("h", 22, "ssh-ed25519", "SHA256:b"),
            HostKeyStatus::Changed
        );
        assert_eq!(
            k.check("h", 22, "ssh-rsa", "SHA256:a"),
            HostKeyStatus::Changed
        );
        assert_eq!(
            k.check("h", 2222, "ssh-ed25519", "SHA256:a"),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn home_is_expanded() {
        assert_eq!(expand_home("/a/b"), PathBuf::from("/a/b"));
        assert_eq!(expand_home("rel/k"), PathBuf::from("rel/k"));
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(expand_home("~/k"), PathBuf::from(home).join("k"));
        }
    }
}
