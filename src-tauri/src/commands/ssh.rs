//! SSH commands (package A): trusting a host key after `Error::SshHostKey`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use dbstudio_core::{ConnectionConfig, Error, Result};
use dbstudio_ssh::{HostKeyStatus, KnownHosts};
use dbstudio_store::{ConnectionService, ssh_secret_account};
use tauri::State;

use super::blocking;
use crate::session::SshEnv;
use crate::state::AppState;

/// Host keys seen during connection attempts, by `(host, port)`: the key a bastion presented
/// last. `ssh_trust_host` only stores a key that was really observed, so a caller cannot make
/// the application trust a key it never saw.
type ObservedKeys = Mutex<HashMap<(String, u16), (String, String)>>;

fn observed() -> &'static ObservedKeys {
    static OBSERVED: OnceLock<ObservedKeys> = OnceLock::new();
    OBSERVED.get_or_init(Default::default)
}

pub(crate) fn record_observed(host: &str, port: u16, key_type: &str, fingerprint: &str) {
    observed().lock().unwrap_or_else(|e| e.into_inner()).insert(
        (host.to_string(), port),
        (key_type.to_string(), fingerprint.to_string()),
    );
}

/// [`KnownHosts`] backed by the `ssh_known_hosts` table of the store.
struct StoreKnownHosts(Arc<ConnectionService>);

impl KnownHosts for StoreKnownHosts {
    fn check(&self, host: &str, port: u16, key_type: &str, fingerprint: &str) -> HostKeyStatus {
        record_observed(host, port, key_type, fingerprint);
        // Any stored key of the host counts, whatever its type: a different key is a change.
        match self.0.store().known_hosts_for(host, port) {
            Ok(keys) if keys.is_empty() => HostKeyStatus::Unknown,
            Ok(keys) => {
                if keys
                    .iter()
                    .any(|k| k.key_type == key_type && k.fingerprint == fingerprint)
                {
                    HostKeyStatus::Known
                } else {
                    HostKeyStatus::Changed
                }
            }
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
    // Only the key the backend saw is trusted; the arguments confirm which one.
    let seen = observed()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(host.clone(), port))
        .cloned();
    if seen != Some((key_type.clone(), fingerprint.clone())) {
        return Err(Error::Invalid(format!(
            "the SSH host key of {host}:{port} was not observed: connect again to see its current key"
        )));
    }
    let svc = state.connections.clone();
    blocking(move || {
        // Trusting a changed key replaces what was trusted before (all key types).
        svc.store().known_host_remove(&host, port)?;
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
    fn a_stored_key_of_another_type_counts_as_changed() {
        let svc = service();
        let k = StoreKnownHosts(svc.clone());
        svc.store()
            .known_host_add("h-types", 22, "ssh-rsa", "SHA256:r")
            .unwrap();
        assert_eq!(
            k.check("h-types", 22, "ssh-ed25519", "SHA256:e"),
            HostKeyStatus::Changed
        );
        assert_eq!(
            k.check("h-types", 22, "ssh-rsa", "SHA256:r"),
            HostKeyStatus::Known
        );
        // several trusted types: any of them is fine
        svc.store()
            .known_host_add("h-types", 22, "ssh-ed25519", "SHA256:e")
            .unwrap();
        assert_eq!(
            k.check("h-types", 22, "ssh-ed25519", "SHA256:e"),
            HostKeyStatus::Known
        );
        assert_eq!(
            k.check("h-types", 22, "ssh-ed25519", "SHA256:other"),
            HostKeyStatus::Changed
        );
    }

    #[tokio::test]
    async fn trust_only_stores_an_observed_key() {
        let (state, _) = crate::state::testing::test_state(dbstudio_core::DriverRegistry::new());
        // never observed
        let err = trust_host_impl(
            &state,
            "ghost".into(),
            22,
            "ssh-ed25519".into(),
            "SHA256:x".into(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
        assert!(
            state
                .connections
                .store()
                .known_hosts_list()
                .unwrap()
                .is_empty()
        );
        // observed through a connection attempt
        let k = StoreKnownHosts(state.connections.clone());
        k.check("seen", 22, "ssh-ed25519", "SHA256:real");
        // a different fingerprint than the observed one is refused
        assert!(
            trust_host_impl(
                &state,
                "seen".into(),
                22,
                "ssh-ed25519".into(),
                "SHA256:fake".into()
            )
            .await
            .is_err()
        );
        assert!(
            trust_host_impl(
                &state,
                "seen".into(),
                22,
                "ssh-rsa".into(),
                "SHA256:real".into()
            )
            .await
            .is_err()
        );
        assert!(
            state
                .connections
                .store()
                .known_hosts_list()
                .unwrap()
                .is_empty()
        );
        trust_host_impl(
            &state,
            "seen".into(),
            22,
            "ssh-ed25519".into(),
            "SHA256:real".into(),
        )
        .await
        .unwrap();
        assert_eq!(
            k.check("seen", 22, "ssh-ed25519", "SHA256:real"),
            HostKeyStatus::Known
        );
        // the key changes, the user trusts the new one: the old one is gone
        assert_eq!(
            k.check("seen", 22, "ssh-ed25519", "SHA256:new"),
            HostKeyStatus::Changed
        );
        trust_host_impl(
            &state,
            "seen".into(),
            22,
            "ssh-ed25519".into(),
            "SHA256:new".into(),
        )
        .await
        .unwrap();
        let keys = state
            .connections
            .store()
            .known_hosts_for("seen", 22)
            .unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].fingerprint, "SHA256:new");
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
        // another key type for a host that has a key is a change, another port is a new host
        assert_eq!(
            k.check("bastion", 22, "ssh-rsa", "SHA256:a"),
            HostKeyStatus::Changed
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
