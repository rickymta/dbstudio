//! SSH commands (package A): trusting a host key after `Error::SshHostKey`.

use std::sync::Arc;

use dbstudio_core::{ConnectionConfig, Error, Result};
use dbstudio_ssh::{HostKeyStatus, KnownHosts};
use dbstudio_store::{ConnectionService, ssh_secret_account};
use tauri::State;

use super::blocking;
use crate::session::SshEnv;
use crate::state::AppState;

/// [`KnownHosts`] backed by the `ssh_known_hosts` table of the store.
struct StoreKnownHosts(Arc<ConnectionService>);

impl KnownHosts for StoreKnownHosts {
    fn check(&self, host: &str, port: u16, key_type: &str, fingerprint: &str) -> HostKeyStatus {
        match self.0.store().known_host_get(host, port, key_type) {
            Ok(Some(k)) if k.fingerprint == fingerprint => HostKeyStatus::Known,
            Ok(Some(_)) => HostKeyStatus::Changed,
            Ok(None) => HostKeyStatus::Unknown,
            Err(e) => {
                // Treat a store failure as "ask the user again" rather than trusting blindly.
                tracing::warn!(error = %e, "reading ssh known hosts failed");
                HostKeyStatus::Unknown
            }
        }
    }
}

/// SSH pieces needed to open `config`: `None` when the profile has no tunnel, otherwise the
/// trusted host keys and the stored tunnel secret (keychain account `<connId>#ssh`).
pub(crate) async fn ssh_env(state: &AppState, config: &ConnectionConfig) -> Result<Option<SshEnv>> {
    if config.ssh.is_none() {
        return Ok(None);
    }
    let secrets = state.secrets.clone();
    let account = ssh_secret_account(config.id);
    let secret = blocking(move || secrets.get_account(&account)).await?;
    Ok(Some(SshEnv {
        known: Arc::new(StoreKnownHosts(state.connections.clone())),
        secret,
    }))
}

/// Store the key the user accepted; the frontend then opens the session again.
pub(crate) async fn trust_host_impl(
    state: &AppState,
    host: String,
    port: u16,
    key_type: String,
    fingerprint: String,
) -> Result<()> {
    let svc = state.connections.clone();
    blocking(move || {
        svc.store()
            .known_host_add(&host, port, &key_type, &fingerprint)
            .map(|_| ())
    })
    .await
}

#[tauri::command]
pub async fn ssh_trust_host(
    state: State<'_, AppState>,
    host: String,
    port: u16,
    key_type: String,
    fingerprint: String,
) -> std::result::Result<(), Error> {
    tracing::debug!(command = "ssh_trust_host", host = %host, port, "ipc call");
    trust_host_impl(&state, host, port, key_type, fingerprint).await
}

#[cfg(test)]
mod tests {
    use dbstudio_core::{DbKind, Secret, SshAuth, SshConfig};
    use dbstudio_store::{MemorySecretStore, SecretStore, Store};

    use super::*;

    fn service() -> Arc<ConnectionService> {
        Arc::new(ConnectionService::new(
            Store::open_in_memory().unwrap(),
            Arc::new(MemorySecretStore::new()),
        ))
    }

    #[test]
    fn store_known_hosts_statuses() {
        let svc = service();
        let k = StoreKnownHosts(svc.clone());
        assert_eq!(
            k.check("bastion", 22, "ssh-ed25519", "SHA256:a"),
            HostKeyStatus::Unknown
        );
        svc.store()
            .known_host_add("bastion", 22, "ssh-ed25519", "SHA256:a")
            .unwrap();
        assert_eq!(
            k.check("bastion", 22, "ssh-ed25519", "SHA256:a"),
            HostKeyStatus::Known
        );
        assert_eq!(
            k.check("bastion", 22, "ssh-ed25519", "SHA256:b"),
            HostKeyStatus::Changed
        );
        // other key type or other port: nothing is known for it
        assert_eq!(
            k.check("bastion", 22, "ssh-rsa", "SHA256:a"),
            HostKeyStatus::Unknown
        );
        assert_eq!(
            k.check("bastion", 2222, "ssh-ed25519", "SHA256:a"),
            HostKeyStatus::Unknown
        );
    }

    #[tokio::test]
    async fn ssh_env_only_for_profiles_with_a_tunnel() {
        let (f, secrets) = crate::state::testing::test_state(dbstudio_core::DriverRegistry::new());
        let mut cfg = ConnectionConfig::new(DbKind::Postgres, "p", "db.internal");
        assert!(ssh_env(&f, &cfg).await.unwrap().is_none());
        cfg.ssh = Some(SshConfig {
            host: "bastion".into(),
            port: 22,
            username: "u".into(),
            auth: SshAuth::Password,
            keepalive_secs: None,
            connect_timeout_ms: None,
        });
        // no secret stored yet
        assert!(ssh_env(&f, &cfg).await.unwrap().unwrap().secret.is_none());
        secrets
            .set_account(&ssh_secret_account(cfg.id), &Secret::new("tunnel-pw"))
            .unwrap();
        let env = ssh_env(&f, &cfg).await.unwrap().unwrap();
        assert_eq!(env.secret.unwrap().expose(), "tunnel-pw");
    }
}
