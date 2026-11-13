//! SSH tunnel (local port forward) used when a connection has `ssh` configured.
//!
//! Scaffold status: [`Tunnel::open`] returns `Error::Unsupported("ssh_tunnel")`. The host key
//! callback types ([`KnownHosts`], [`HostKeyStatus`]) are final. Package A implements the tunnel
//! with `russh`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Mutex;

use dbstudio_core::{Error, Result, Secret, SshConfig};

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

/// An open tunnel. Dropping it closes the listener and the SSH session.
#[derive(Debug)]
pub struct Tunnel {
    local_addr: SocketAddr,
}

impl Tunnel {
    /// Connect to the bastion in `cfg`, authenticate (`secret` = password or key passphrase),
    /// verify the host key through `known`, and listen on `127.0.0.1:0` forwarding every
    /// accepted connection to `target`.
    ///
    /// Scaffold: `Unsupported("ssh_tunnel")`.
    pub async fn open(
        cfg: &SshConfig,
        secret: Option<&Secret>,
        target: (String, u16),
        known: &dyn KnownHosts,
    ) -> Result<Tunnel> {
        let _ = (cfg, secret, target, known);
        Err(Error::Unsupported("ssh_tunnel".into()))
    }

    /// Local address connections are forwarded from.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::SshAuth;

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

    #[tokio::test]
    async fn stub_contract_open_is_unsupported() {
        let cfg = SshConfig {
            host: "bastion".into(),
            port: 22,
            username: "u".into(),
            auth: SshAuth::Agent,
            keepalive_secs: None,
            connect_timeout_ms: None,
        };
        let err = Tunnel::open(&cfg, None, ("db".into(), 5432), &MemoryKnownHosts::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unsupported(ref s) if s == "ssh_tunnel"));
    }

    #[test]
    fn russh_is_linked() {
        // Keeps the dependency honest until the tunnel is implemented.
        let _ = std::any::type_name::<russh::client::Config>();
    }
}
