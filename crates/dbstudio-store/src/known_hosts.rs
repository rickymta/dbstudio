//! Trusted SSH host keys (`ssh_known_hosts`). Package A owns this file.

use dbstudio_core::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::db::{Store, db_err, now_iso};

/// A trusted host key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownHost {
    /// Host name or address as typed in the connection.
    pub host: String,
    /// SSH port.
    pub port: u16,
    /// Key algorithm (`ssh-ed25519`, `ecdsa-sha2-nistp256`, `ssh-rsa`, ...).
    pub key_type: String,
    /// `SHA256:<base64>` fingerprint.
    pub fingerprint: String,
    /// ISO-8601 time the key was trusted.
    pub added_at: String,
}

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<KnownHost> {
    let port: i64 = row.get(1)?;
    Ok(KnownHost {
        host: row.get(0)?,
        port: u16::try_from(port).unwrap_or(0),
        key_type: row.get(2)?,
        fingerprint: row.get(3)?,
        added_at: row.get(4)?,
    })
}

impl Store {
    /// The trusted key of `host:port` for `key_type`, if any.
    pub fn known_host_get(
        &self,
        host: &str,
        port: u16,
        key_type: &str,
    ) -> Result<Option<KnownHost>> {
        self.lock()
            .query_row(
                "SELECT host, port, key_type, fingerprint, added_at FROM ssh_known_hosts
                 WHERE host = ?1 AND port = ?2 AND key_type = ?3",
                params![host, i64::from(port), key_type],
                from_row,
            )
            .optional()
            .map_err(db_err)
    }

    /// Every trusted key of `host:port` (any key type).
    pub fn known_hosts_for(&self, host: &str, port: u16) -> Result<Vec<KnownHost>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT host, port, key_type, fingerprint, added_at FROM ssh_known_hosts
                 WHERE host = ?1 AND port = ?2 ORDER BY key_type",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![host, i64::from(port)], from_row)
            .map_err(db_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(db_err)
    }

    /// Every trusted key, ordered by host, port and key type.
    pub fn known_hosts_list(&self) -> Result<Vec<KnownHost>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT host, port, key_type, fingerprint, added_at FROM ssh_known_hosts
                 ORDER BY host, port, key_type",
            )
            .map_err(db_err)?;
        let rows = stmt.query_map([], from_row).map_err(db_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(db_err)
    }

    /// Trust a key; replaces the fingerprint when `host:port` already has one for the type.
    pub fn known_host_add(
        &self,
        host: &str,
        port: u16,
        key_type: &str,
        fingerprint: &str,
    ) -> Result<KnownHost> {
        let added_at = now_iso();
        self.lock()
            .execute(
                "INSERT INTO ssh_known_hosts (host, port, key_type, fingerprint, added_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(host, port, key_type) DO UPDATE SET
                    fingerprint = excluded.fingerprint, added_at = excluded.added_at",
                params![host, i64::from(port), key_type, fingerprint, added_at],
            )
            .map_err(db_err)?;
        Ok(KnownHost {
            host: host.into(),
            port,
            key_type: key_type.into(),
            fingerprint: fingerprint.into(),
            added_at,
        })
    }

    /// Forget the keys of `host:port` (all types); returns how many were removed.
    pub fn known_host_remove(&self, host: &str, port: u16) -> Result<u64> {
        let n = self
            .lock()
            .execute(
                "DELETE FROM ssh_known_hosts WHERE host = ?1 AND port = ?2",
                params![host, i64::from(port)],
            )
            .map_err(db_err)?;
        Ok(n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_get_replace_remove() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.known_host_get("h", 22, "ssh-ed25519").unwrap().is_none());
        let added = s
            .known_host_add("h", 22, "ssh-ed25519", "SHA256:aaa")
            .unwrap();
        s.known_host_add("h", 22, "ssh-rsa", "SHA256:bbb").unwrap();
        s.known_host_add("h", 2222, "ssh-ed25519", "SHA256:ccc")
            .unwrap();
        assert_eq!(
            s.known_host_get("h", 22, "ssh-ed25519").unwrap(),
            Some(added)
        );
        assert_eq!(s.known_hosts_for("h", 22).unwrap().len(), 2);
        assert_eq!(s.known_hosts_list().unwrap().len(), 3);

        // A changed key replaces the old fingerprint instead of duplicating.
        s.known_host_add("h", 22, "ssh-ed25519", "SHA256:new")
            .unwrap();
        assert_eq!(s.known_hosts_list().unwrap().len(), 3);
        assert_eq!(
            s.known_host_get("h", 22, "ssh-ed25519")
                .unwrap()
                .unwrap()
                .fingerprint,
            "SHA256:new"
        );

        assert_eq!(s.known_host_remove("h", 22).unwrap(), 2);
        assert_eq!(s.known_host_remove("h", 22).unwrap(), 0);
        assert_eq!(s.known_hosts_list().unwrap().len(), 1);
    }
}
