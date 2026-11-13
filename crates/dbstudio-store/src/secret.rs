//! Secret (password) storage. Secrets live only in the OS keychain, never in SQLite or files.

use std::collections::HashMap;
use std::sync::Mutex;

use dbstudio_core::{Error, Result, Secret};
use uuid::Uuid;

/// Keychain service name under which every DBStudio secret is stored.
pub const KEYRING_SERVICE: &str = "dev.dbstudio.desktop";

/// Keychain account of the SSH secret (password or key passphrase) of a connection.
pub fn ssh_secret_account(conn_id: Uuid) -> String {
    format!("{conn_id}#ssh")
}

/// Keychain account of the encryption passphrase of a backup job.
pub fn backup_secret_account(job_id: Uuid) -> String {
    format!("backup:{job_id}")
}

/// Storage for secrets. The connection password of a profile lives under the account
/// `<connId>`; other secrets use their own account names ([`ssh_secret_account`],
/// [`backup_secret_account`]).
pub trait SecretStore: Send + Sync + std::fmt::Debug {
    /// The secret stored under `account`, or `None` when nothing is stored.
    fn get_account(&self, account: &str) -> Result<Option<Secret>>;
    /// Store or replace the secret under `account`.
    fn set_account(&self, account: &str, secret: &Secret) -> Result<()>;
    /// Remove the secret under `account`; succeeds when nothing was stored.
    fn delete_account(&self, account: &str) -> Result<()>;

    /// The connection password, or `None` when nothing is stored.
    fn get(&self, id: Uuid) -> Result<Option<Secret>> {
        self.get_account(&id.to_string())
    }
    /// Store or replace the connection password.
    fn set(&self, id: Uuid, secret: &Secret) -> Result<()> {
        self.set_account(&id.to_string(), secret)
    }
    /// Remove the connection password; succeeds when nothing was stored.
    fn delete(&self, id: Uuid) -> Result<()> {
        self.delete_account(&id.to_string())
    }
}

/// [`SecretStore`] backed by the OS credential store (macOS Keychain, Windows Credential
/// Manager, Linux Secret Service).
#[derive(Debug, Clone)]
pub struct KeyringSecretStore {
    service: String,
}

impl KeyringSecretStore {
    /// Use the standard DBStudio service name.
    pub fn new() -> Self {
        Self::with_service(KEYRING_SERVICE)
    }

    /// Use a custom service name.
    pub fn with_service(service: impl Into<String>) -> Self {
        KeyringSecretStore {
            service: service.into(),
        }
    }

    fn entry(&self, account: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, account).map_err(keychain_err)
    }
}

impl Default for KeyringSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

fn keychain_err(e: keyring::Error) -> Error {
    Error::Storage(format!(
        "the OS keychain is not available or refused access: {e}"
    ))
}

impl SecretStore for KeyringSecretStore {
    fn get_account(&self, account: &str) -> Result<Option<Secret>> {
        match self.entry(account)?.get_password() {
            Ok(value) => Ok(Some(Secret::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keychain_err(e)),
        }
    }

    fn set_account(&self, account: &str, secret: &Secret) -> Result<()> {
        self.entry(account)?
            .set_password(secret.expose())
            .map_err(keychain_err)
    }

    fn delete_account(&self, account: &str) -> Result<()> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(keychain_err(e)),
        }
    }
}

/// In-memory [`SecretStore`] for tests. Can be switched to a failing mode that mimics an
/// unavailable keychain.
#[derive(Default)]
pub struct MemorySecretStore {
    secrets: Mutex<HashMap<String, Secret>>,
    unavailable: Mutex<bool>,
}

impl std::fmt::Debug for MemorySecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySecretStore").finish_non_exhaustive()
    }
}

impl MemorySecretStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Make every later operation fail (or work again) as if the keychain were locked.
    pub fn set_unavailable(&self, unavailable: bool) {
        *self.unavailable.lock().unwrap_or_else(|e| e.into_inner()) = unavailable;
    }

    /// Number of stored secrets (ignores availability).
    pub fn len(&self) -> usize {
        self.secrets.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// True when no secret is stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn check(&self) -> Result<()> {
        if *self.unavailable.lock().unwrap_or_else(|e| e.into_inner()) {
            Err(Error::Storage(
                "the OS keychain is not available or refused access: memory store unavailable"
                    .into(),
            ))
        } else {
            Ok(())
        }
    }
}

impl SecretStore for MemorySecretStore {
    fn get_account(&self, account: &str) -> Result<Option<Secret>> {
        self.check()?;
        Ok(self
            .secrets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
            .cloned())
    }

    fn set_account(&self, account: &str, secret: &Secret) -> Result<()> {
        self.check()?;
        self.secrets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account.to_string(), secret.clone());
        Ok(())
    }

    fn delete_account(&self, account: &str) -> Result<()> {
        self.check()?;
        self.secrets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_roundtrip() {
        let s = MemorySecretStore::new();
        let id = Uuid::new_v4();
        assert!(s.get(id).unwrap().is_none());
        s.set(id, &Secret::new("pw")).unwrap();
        assert_eq!(s.get(id).unwrap().unwrap().expose(), "pw");
        s.set(id, &Secret::new("pw2")).unwrap();
        assert_eq!(s.get(id).unwrap().unwrap().expose(), "pw2");
        s.delete(id).unwrap();
        s.delete(id).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn account_helpers_and_isolation() {
        let id = Uuid::new_v4();
        assert_eq!(ssh_secret_account(id), format!("{id}#ssh"));
        assert_eq!(backup_secret_account(id), format!("backup:{id}"));
        let s = MemorySecretStore::new();
        s.set(id, &Secret::new("db")).unwrap();
        s.set_account(&ssh_secret_account(id), &Secret::new("tunnel"))
            .unwrap();
        s.set_account(&backup_secret_account(id), &Secret::new("bk"))
            .unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s.get(id).unwrap().unwrap().expose(), "db");
        assert_eq!(
            s.get_account(&ssh_secret_account(id))
                .unwrap()
                .unwrap()
                .expose(),
            "tunnel"
        );
        s.delete_account(&ssh_secret_account(id)).unwrap();
        assert_eq!(s.len(), 2);
        assert!(s.get(id).unwrap().is_some());
    }

    #[test]
    fn unavailable_store_reports_storage_errors() {
        let s = MemorySecretStore::new();
        s.set_unavailable(true);
        let id = Uuid::new_v4();
        assert!(matches!(s.get(id), Err(Error::Storage(_))));
        assert!(matches!(
            s.set(id, &Secret::new("x")),
            Err(Error::Storage(_))
        ));
        assert!(matches!(s.delete(id), Err(Error::Storage(_))));
    }

    /// Removes the test credential even when an assertion fails.
    struct Cleanup(KeyringSecretStore, Uuid);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.delete(self.1);
        }
    }

    /// Talks to the real OS keychain: run with `DBSTUDIO_KEYCHAIN_IT=1` (macOS may prompt).
    #[test]
    fn real_keychain_roundtrip() {
        if std::env::var("DBSTUDIO_KEYCHAIN_IT").as_deref() != Ok("1") {
            return;
        }
        let store = KeyringSecretStore::with_service(format!("{KEYRING_SERVICE}.test"));
        let id = Uuid::new_v4();
        let _cleanup = Cleanup(store.clone(), id);

        assert!(store.get(id).unwrap().is_none());
        store.set(id, &Secret::new("first")).unwrap();
        assert_eq!(store.get(id).unwrap().unwrap().expose(), "first");
        store
            .set(id, &Secret::new("p\u{e4}ss w\u{f6}rd \u{1f510}"))
            .unwrap();
        assert_eq!(
            store.get(id).unwrap().unwrap().expose(),
            "p\u{e4}ss w\u{f6}rd \u{1f510}"
        );
        store.delete(id).unwrap();
        assert!(store.get(id).unwrap().is_none());
        store.delete(id).unwrap();
    }
}
