//! SQLite persistence of connection groups and profiles (no secrets are stored here).

use std::collections::{BTreeMap, HashSet};
use std::fmt::Display;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use dbstudio_core::{ConnectionConfig, DbKind, Error, Result, TlsConfig};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::model::{ConnectionGroup, ConnectionProfile, ConnectionsSnapshot};
use crate::settings::AppSettings;
use crate::validate::{validate_group, validate_profile};

/// Schema migrations. Entry `i` upgrades the database from version `i` to `i + 1`; the version
/// is tracked in `PRAGMA user_version`. Never edit an existing entry: append a new one.
const MIGRATIONS: &[&str] = &[
    // v1: groups and profiles.
    "CREATE TABLE connection_groups (
        id         TEXT PRIMARY KEY NOT NULL,
        name       TEXT NOT NULL,
        parent_id  TEXT REFERENCES connection_groups(id) ON DELETE SET NULL,
        sort_order INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE connections (
        id                    TEXT PRIMARY KEY NOT NULL,
        name                  TEXT NOT NULL,
        kind                  TEXT NOT NULL,
        host                  TEXT NOT NULL,
        port                  INTEGER NOT NULL,
        database_name         TEXT,
        username              TEXT,
        tls_mode              TEXT NOT NULL,
        tls_ca_cert_path      TEXT,
        tls_client_cert_path  TEXT,
        tls_client_key_path   TEXT,
        connect_timeout_ms    INTEGER,
        read_only             INTEGER NOT NULL DEFAULT 0,
        environment           TEXT,
        options               TEXT NOT NULL DEFAULT '{}',
        group_id              TEXT REFERENCES connection_groups(id) ON DELETE SET NULL,
        color                 TEXT,
        save_password         INTEGER NOT NULL DEFAULT 0,
        has_password          INTEGER NOT NULL DEFAULT 0,
        sort_order            INTEGER NOT NULL DEFAULT 0,
        created_at            TEXT NOT NULL,
        updated_at            TEXT NOT NULL
    );
    CREATE INDEX idx_connections_group ON connections(group_id);
    CREATE INDEX idx_groups_parent ON connection_groups(parent_id);",
    // v2: application settings (key -> JSON document).
    "CREATE TABLE settings (
        key   TEXT PRIMARY KEY NOT NULL,
        value TEXT NOT NULL
    );",
];

const PROFILE_COLUMNS: &str = "id, name, kind, host, port, database_name, username, tls_mode, \
    tls_ca_cert_path, tls_client_cert_path, tls_client_key_path, connect_timeout_ms, read_only, \
    environment, options, group_id, color, save_password, has_password, sort_order, created_at, \
    updated_at";

fn db_err(e: impl Display) -> Error {
    Error::Storage(format!("database error: {e}"))
}

fn invalid(msg: impl Display) -> Error {
    Error::Storage(format!("invalid data: {msg}"))
}

/// Current time as an ISO-8601 (RFC 3339, UTC) string.
pub(crate) fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Apply every migration newer than the database's `user_version`.
pub(crate) fn migrate(conn: &mut Connection, migrations: &[&str]) -> Result<()> {
    let current: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(db_err)?;
    let latest = migrations.len() as i64;
    if current > latest {
        return Err(Error::Storage(format!(
            "database schema version {current} is newer than this application supports \
             ({latest}); please update DBStudio"
        )));
    }
    for (index, sql) in migrations.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute_batch(sql)
            .map_err(|e| Error::Storage(format!("migration to v{} failed: {e}", index + 1)))?;
        tx.pragma_update(None, "user_version", index as i64 + 1)
            .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
    }
    Ok(())
}

fn to_text<T: Serialize>(value: &T) -> Result<String> {
    match serde_json::to_value(value).map_err(invalid)? {
        serde_json::Value::String(s) => Ok(s),
        other => Err(invalid(format!("expected a string, got {other}"))),
    }
}

fn from_text<T: DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(text.to_string()))
        .map_err(|e| invalid(format!("unknown value `{text}`: {e}")))
}

fn parse_uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text).map_err(|e| invalid(format!("bad uuid `{text}`: {e}")))
}

fn opt_uuid(text: Option<String>) -> Result<Option<Uuid>> {
    text.as_deref().map(parse_uuid).transpose()
}

fn profile_from_row(row: &Row<'_>) -> rusqlite::Result<Result<ConnectionProfile>> {
    let id: String = row.get("id")?;
    let kind: String = row.get("kind")?;
    let tls_mode: String = row.get("tls_mode")?;
    let environment: Option<String> = row.get("environment")?;
    let options: String = row.get("options")?;
    let group_id: Option<String> = row.get("group_id")?;
    let port: i64 = row.get("port")?;
    let timeout: Option<i64> = row.get("connect_timeout_ms")?;
    let name: String = row.get("name")?;
    let host: String = row.get("host")?;
    let database: Option<String> = row.get("database_name")?;
    let username: Option<String> = row.get("username")?;
    let ca: Option<String> = row.get("tls_ca_cert_path")?;
    let cert: Option<String> = row.get("tls_client_cert_path")?;
    let key: Option<String> = row.get("tls_client_key_path")?;
    let read_only: bool = row.get("read_only")?;
    let color: Option<String> = row.get("color")?;
    let save_password: bool = row.get("save_password")?;
    let has_password: bool = row.get("has_password")?;
    let sort_order: i64 = row.get("sort_order")?;
    let created_at: String = row.get("created_at")?;
    let updated_at: String = row.get("updated_at")?;
    Ok((|| {
        Ok(ConnectionProfile {
            config: ConnectionConfig {
                id: parse_uuid(&id)?,
                name,
                kind: from_text::<DbKind>(&kind)?,
                host,
                port: u16::try_from(port).map_err(|_| invalid(format!("bad port {port}")))?,
                database,
                username,
                tls: TlsConfig {
                    mode: from_text(&tls_mode)?,
                    ca_cert_path: ca,
                    client_cert_path: cert,
                    client_key_path: key,
                },
                connect_timeout_ms: timeout.map(|t| t.max(0) as u64),
                read_only,
                environment: environment.as_deref().map(from_text).transpose()?,
                options: serde_json::from_str::<BTreeMap<String, String>>(&options)
                    .map_err(|e| invalid(format!("bad options json: {e}")))?,
            },
            group_id: opt_uuid(group_id)?,
            color,
            save_password,
            has_password,
            sort_order,
            created_at,
            updated_at,
        })
    })())
}

fn group_from_row(row: &Row<'_>) -> rusqlite::Result<Result<ConnectionGroup>> {
    let id: String = row.get(0)?;
    let name: String = row.get(1)?;
    let parent: Option<String> = row.get(2)?;
    let sort_order: i64 = row.get(3)?;
    Ok((|| {
        Ok(ConnectionGroup {
            id: parse_uuid(&id)?,
            name,
            parent_id: opt_uuid(parent)?,
            sort_order,
        })
    })())
}

/// SQLite-backed store of connection groups and profiles.
pub struct Store {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Open (creating if needed) the database at `path` and migrate it to the latest schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        let path = path.as_ref();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).map_err(db_err)?;
        Self::init(conn, MIGRATIONS)
    }

    /// Open a private in-memory database (for tests).
    pub fn open_in_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory().map_err(db_err)?, MIGRATIONS)
    }

    pub(crate) fn init(mut conn: Connection, migrations: &[&str]) -> Result<Store> {
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_err)?;
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(db_err)?;
        // Returns the resulting mode ("memory" for in-memory databases), so use query_row.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .map_err(db_err)?;
        migrate(&mut conn, migrations)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A poisoned lock only means another thread panicked mid-operation; SQLite's own
        // transactions keep the data consistent, so continue.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Schema version of the open database.
    pub fn schema_version(&self) -> Result<u32> {
        let v: i64 = self
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(db_err)?;
        Ok(v as u32)
    }

    /// Stored settings (defaults for anything missing or unreadable), normalized.
    pub fn get_settings(&self) -> Result<AppSettings> {
        let text: Option<String> = self
            .lock()
            .query_row("SELECT value FROM settings WHERE key = 'app'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(db_err)?;
        Ok(text
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .map(|v| AppSettings::from_value_lenient(&v))
            .unwrap_or_default())
    }

    /// Normalize and persist settings; returns the stored value.
    pub fn set_settings(&self, settings: AppSettings) -> Result<AppSettings> {
        let settings = settings.normalized();
        let json = serde_json::to_string(&settings).map_err(invalid)?;
        self.lock()
            .execute(
                "INSERT INTO settings (key, value) VALUES ('app', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![json],
            )
            .map_err(db_err)?;
        Ok(settings)
    }

    /// All groups and profiles ordered by `sort_order`, then name.
    pub fn snapshot(&self) -> Result<ConnectionsSnapshot> {
        let conn = self.lock();
        Ok(ConnectionsSnapshot {
            groups: query_groups(&conn)?,
            profiles: query_profiles(&conn)?,
        })
    }

    /// Fetch one profile or fail with `NotFound`.
    pub fn get_profile(&self, id: Uuid) -> Result<ConnectionProfile> {
        find_profile(&self.lock(), id)?.ok_or_else(|| Error::NotFound(format!("connection {id}")))
    }

    /// Fetch one profile if it exists.
    pub fn find_profile(&self, id: Uuid) -> Result<Option<ConnectionProfile>> {
        find_profile(&self.lock(), id)
    }

    /// Insert or update (by id) a profile. `created_at` of an existing profile is preserved,
    /// `updated_at` is refreshed; a new profile is appended after the existing ones.
    pub fn save_profile(&self, profile: &ConnectionProfile) -> Result<ConnectionProfile> {
        let mut profile = profile.clone();
        // An empty color means "no color".
        profile.color = profile.color.filter(|c| !c.is_empty());
        validate_profile(&profile)?;
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        if let Some(group) = profile.group_id {
            ensure_group(&tx, group)?;
        }
        let existing = find_profile(&tx, profile.config.id)?;
        let now = now_iso();
        let mut saved = profile.clone();
        saved.updated_at = now.clone();
        match &existing {
            Some(old) => saved.created_at = old.created_at.clone(),
            None => {
                saved.created_at = now;
                saved.sort_order = next_profile_order(&tx)?;
            }
        }
        write_profile(&tx, &saved)?;
        tx.commit().map_err(db_err)?;
        Ok(saved)
    }

    /// Record whether a secret is stored for the profile.
    pub fn set_has_password(&self, id: Uuid, has_password: bool) -> Result<()> {
        let n = self
            .lock()
            .execute(
                "UPDATE connections SET has_password = ?1 WHERE id = ?2",
                params![has_password, id.to_string()],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("connection {id}")));
        }
        Ok(())
    }

    /// Delete a profile.
    pub fn delete_profile(&self, id: Uuid) -> Result<()> {
        let n = self
            .lock()
            .execute(
                "DELETE FROM connections WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("connection {id}")));
        }
        Ok(())
    }

    /// Copy a profile under a fresh id with the name suffix ` (copy)`, in the same group.
    /// The copy never claims to have a password; the caller copies the secret if needed.
    pub fn duplicate_profile(&self, id: Uuid) -> Result<ConnectionProfile> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        let src =
            find_profile(&tx, id)?.ok_or_else(|| Error::NotFound(format!("connection {id}")))?;
        let now = now_iso();
        let mut copy = src;
        copy.config.id = Uuid::new_v4();
        copy.config.name = format!("{} (copy)", copy.config.name);
        copy.has_password = false;
        copy.sort_order = next_profile_order(&tx)?;
        copy.created_at = now.clone();
        copy.updated_at = now;
        write_profile(&tx, &copy)?;
        tx.commit().map_err(db_err)?;
        Ok(copy)
    }

    /// Insert or update (by id) a group. The parent must exist and must not create a cycle.
    pub fn save_group(&self, group: &ConnectionGroup) -> Result<ConnectionGroup> {
        validate_group(group)?;
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        if let Some(parent) = group.parent_id {
            ensure_group(&tx, parent)?;
            check_no_cycle(&tx, group.id, parent)?;
        }
        tx.execute(
            "INSERT INTO connection_groups (id, name, parent_id, sort_order)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                parent_id = excluded.parent_id,
                sort_order = excluded.sort_order",
            params![
                group.id.to_string(),
                group.name,
                group.parent_id.map(|p| p.to_string()),
                group.sort_order
            ],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(group.clone())
    }

    /// Delete a group; its profiles and subgroups move up to its parent.
    pub fn delete_group(&self, id: Uuid) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        let parent: Option<String> = tx
            .query_row(
                "SELECT parent_id FROM connection_groups WHERE id = ?1",
                params![id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?
            .ok_or_else(|| Error::NotFound(format!("group {id}")))?;
        let id = id.to_string();
        tx.execute(
            "UPDATE connection_groups SET parent_id = ?1 WHERE parent_id = ?2",
            params![parent, id],
        )
        .map_err(db_err)?;
        tx.execute(
            "UPDATE connections SET group_id = ?1 WHERE group_id = ?2",
            params![parent, id],
        )
        .map_err(db_err)?;
        tx.execute("DELETE FROM connection_groups WHERE id = ?1", params![id])
            .map_err(db_err)?;
        tx.commit().map_err(db_err)
    }

    /// Insert groups and profiles in one transaction, keeping the ids and timestamps they
    /// carry. Used by import after fresh ids were assigned.
    pub(crate) fn insert_all(
        &self,
        groups: &[ConnectionGroup],
        profiles: &[ConnectionProfile],
    ) -> Result<()> {
        for p in profiles {
            validate_profile(p)?;
        }
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        // Insert parents without their parent first, then link, so order does not matter.
        for g in groups {
            validate_group(g)?;
            tx.execute(
                "INSERT INTO connection_groups (id, name, parent_id, sort_order)
                 VALUES (?1, ?2, NULL, ?3)",
                params![g.id.to_string(), g.name, g.sort_order],
            )
            .map_err(db_err)?;
        }
        for g in groups {
            tx.execute(
                "UPDATE connection_groups SET parent_id = ?1 WHERE id = ?2",
                params![g.parent_id.map(|p| p.to_string()), g.id.to_string()],
            )
            .map_err(db_err)?;
        }
        for p in profiles {
            write_profile(&tx, p)?;
        }
        tx.commit().map_err(db_err)
    }
}

fn query_groups(conn: &Connection) -> Result<Vec<ConnectionGroup>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, parent_id, sort_order FROM connection_groups
             ORDER BY sort_order, name COLLATE NOCASE, id",
        )
        .map_err(db_err)?;
    let rows = stmt.query_map([], group_from_row).map_err(db_err)?;
    rows.map(|r| r.map_err(db_err)?).collect()
}

fn query_profiles(conn: &Connection) -> Result<Vec<ConnectionProfile>> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {PROFILE_COLUMNS} FROM connections
             ORDER BY sort_order, name COLLATE NOCASE, id"
        ))
        .map_err(db_err)?;
    let rows = stmt.query_map([], profile_from_row).map_err(db_err)?;
    rows.map(|r| r.map_err(db_err)?).collect()
}

fn find_profile(conn: &Connection, id: Uuid) -> Result<Option<ConnectionProfile>> {
    conn.query_row(
        &format!("SELECT {PROFILE_COLUMNS} FROM connections WHERE id = ?1"),
        params![id.to_string()],
        profile_from_row,
    )
    .optional()
    .map_err(db_err)?
    .transpose()
}

fn next_profile_order(tx: &Transaction<'_>) -> Result<i64> {
    let max: Option<i64> = tx
        .query_row("SELECT MAX(sort_order) FROM connections", [], |r| r.get(0))
        .map_err(db_err)?;
    Ok(max.map_or(0, |m| m + 1))
}

fn ensure_group(conn: &Connection, id: Uuid) -> Result<()> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM connection_groups WHERE id = ?1)",
            params![id.to_string()],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    if exists {
        Ok(())
    } else {
        Err(Error::NotFound(format!("group {id}")))
    }
}

/// Fail when making `parent` the parent of `group` would create a loop.
fn check_no_cycle(conn: &Connection, group: Uuid, parent: Uuid) -> Result<()> {
    let mut seen = HashSet::new();
    let mut cursor = Some(parent);
    while let Some(current) = cursor {
        if current == group {
            return Err(invalid(
                "a group cannot be moved into itself or its descendants",
            ));
        }
        if !seen.insert(current) {
            break; // pre-existing loop elsewhere; do not spin forever
        }
        let next: Option<String> = conn
            .query_row(
                "SELECT parent_id FROM connection_groups WHERE id = ?1",
                params![current.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?
            .flatten();
        cursor = opt_uuid(next)?;
    }
    Ok(())
}

fn write_profile(tx: &Transaction<'_>, p: &ConnectionProfile) -> Result<()> {
    let c = &p.config;
    let options = serde_json::to_string(&c.options).map_err(invalid)?;
    let environment = c.environment.as_ref().map(to_text).transpose()?;
    tx.execute(
        &format!(
            "INSERT INTO connections ({PROFILE_COLUMNS}) VALUES
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
              ?19, ?20, ?21, ?22)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name, kind = excluded.kind, host = excluded.host,
                port = excluded.port, database_name = excluded.database_name,
                username = excluded.username, tls_mode = excluded.tls_mode,
                tls_ca_cert_path = excluded.tls_ca_cert_path,
                tls_client_cert_path = excluded.tls_client_cert_path,
                tls_client_key_path = excluded.tls_client_key_path,
                connect_timeout_ms = excluded.connect_timeout_ms,
                read_only = excluded.read_only, environment = excluded.environment,
                options = excluded.options, group_id = excluded.group_id,
                color = excluded.color, save_password = excluded.save_password,
                has_password = excluded.has_password, sort_order = excluded.sort_order,
                updated_at = excluded.updated_at"
        ),
        params![
            c.id.to_string(),
            c.name,
            to_text(&c.kind)?,
            c.host,
            i64::from(c.port),
            c.database,
            c.username,
            to_text(&c.tls.mode)?,
            c.tls.ca_cert_path,
            c.tls.client_cert_path,
            c.tls.client_key_path,
            c.connect_timeout_ms.map(|t| t as i64),
            c.read_only,
            environment,
            options,
            p.group_id.map(|g| g.to_string()),
            p.color,
            p.save_password,
            p.has_password,
            p.sort_order,
            p.created_at,
            p.updated_at,
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{Environment, TlsMode};

    fn profile(name: &str) -> ConnectionProfile {
        ConnectionProfile {
            config: ConnectionConfig::new(DbKind::Postgres, name, "localhost"),
            group_id: None,
            color: None,
            save_password: false,
            has_password: false,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn group(name: &str, parent: Option<Uuid>) -> ConnectionGroup {
        ConnectionGroup {
            id: Uuid::new_v4(),
            name: name.into(),
            parent_id: parent,
            sort_order: 0,
        }
    }

    #[test]
    fn fresh_database_is_at_latest_version() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len() as u32);
        let snap = s.snapshot().unwrap();
        assert!(snap.groups.is_empty() && snap.profiles.is_empty());
    }

    #[test]
    fn save_roundtrips_every_field() {
        let s = Store::open_in_memory().unwrap();
        let g = s.save_group(&group("Work", None)).unwrap();
        let mut p = profile("prod");
        p.group_id = Some(g.id);
        p.color = Some("#e5484d".into());
        p.save_password = true;
        p.config.database = Some("app".into());
        p.config.username = Some("admin".into());
        p.config.tls = TlsConfig {
            mode: TlsMode::VerifyFull,
            ca_cert_path: Some("/ca.pem".into()),
            client_cert_path: Some("/c.pem".into()),
            client_key_path: Some("/k.pem".into()),
        };
        p.config.connect_timeout_ms = Some(2500);
        p.config.read_only = true;
        p.config.environment = Some(Environment::Production);
        p.config
            .options
            .insert("application_name".into(), "dbs".into());
        let saved = s.save_profile(&p).unwrap();
        let loaded = s.get_profile(p.config.id).unwrap();
        assert_eq!(loaded, saved);
        assert_eq!(loaded.config, p.config);
        assert_eq!(loaded.group_id, p.group_id);
        assert!(!loaded.created_at.is_empty());
        assert_eq!(s.snapshot().unwrap().profiles, vec![loaded]);
    }

    #[test]
    fn upsert_keeps_created_at_and_updates_updated_at() {
        let s = Store::open_in_memory().unwrap();
        let first = s.save_profile(&profile("a")).unwrap();
        // Force an older created_at so the comparison is not timing dependent.
        s.lock()
            .execute(
                "UPDATE connections SET created_at = '2020-01-01T00:00:00Z', \
                 updated_at = '2020-01-01T00:00:00Z'",
                [],
            )
            .unwrap();
        let mut edit = first.clone();
        edit.config.name = "renamed".into();
        edit.created_at = "garbage".into();
        let second = s.save_profile(&edit).unwrap();
        assert_eq!(second.created_at, "2020-01-01T00:00:00Z");
        assert_ne!(second.updated_at, "2020-01-01T00:00:00Z");
        assert_eq!(s.snapshot().unwrap().profiles.len(), 1);
        assert_eq!(
            s.get_profile(first.config.id).unwrap().config.name,
            "renamed"
        );
    }

    #[test]
    fn validation_rules() {
        let s = Store::open_in_memory().unwrap();
        let mut p = profile("  ");
        assert!(matches!(s.save_profile(&p), Err(Error::Storage(_))));
        p.config.name = "ok".into();
        p.config.host = " ".into();
        assert!(matches!(s.save_profile(&p), Err(Error::Storage(_))));
        p.config.host = "h".into();
        p.config.port = 0;
        assert!(matches!(s.save_profile(&p), Err(Error::Storage(_))));
        p.config.port = 1;
        p.group_id = Some(Uuid::new_v4());
        assert!(matches!(s.save_profile(&p), Err(Error::NotFound(_))));
        p.group_id = None;
        s.save_profile(&p).unwrap();
        assert!(matches!(
            s.save_group(&group("", None)),
            Err(Error::Storage(_))
        ));
        assert!(matches!(
            s.save_group(&group("x", Some(Uuid::new_v4()))),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn snapshot_orders_by_sort_order_then_name() {
        let s = Store::open_in_memory().unwrap();
        for (name, order) in [("b", 1), ("a", 1), ("z", 0)] {
            let mut g = group(name, None);
            g.sort_order = order;
            s.save_group(&g).unwrap();
        }
        let names: Vec<_> = s
            .snapshot()
            .unwrap()
            .groups
            .into_iter()
            .map(|g| g.name)
            .collect();
        assert_eq!(names, ["z", "a", "b"]);
        // New profiles are appended in creation order regardless of the sort order sent.
        let p1 = s.save_profile(&profile("second-alpha")).unwrap();
        let p2 = s.save_profile(&profile("first-alpha")).unwrap();
        assert!(p1.sort_order < p2.sort_order);
    }

    #[test]
    fn delete_and_not_found() {
        let s = Store::open_in_memory().unwrap();
        let p = s.save_profile(&profile("a")).unwrap();
        s.delete_profile(p.config.id).unwrap();
        assert!(matches!(
            s.delete_profile(p.config.id),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            s.get_profile(p.config.id),
            Err(Error::NotFound(_))
        ));
        assert!(s.find_profile(p.config.id).unwrap().is_none());
        assert!(matches!(
            s.delete_group(Uuid::new_v4()),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            s.set_has_password(p.config.id, true),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn duplicate_copies_profile_into_same_group() {
        let s = Store::open_in_memory().unwrap();
        let g = s.save_group(&group("G", None)).unwrap();
        let mut p = profile("db");
        p.group_id = Some(g.id);
        p.has_password = true;
        let p = s.save_profile(&p).unwrap();
        let copy = s.duplicate_profile(p.config.id).unwrap();
        assert_ne!(copy.config.id, p.config.id);
        assert_eq!(copy.config.name, "db (copy)");
        assert_eq!(copy.group_id, Some(g.id));
        assert_eq!(copy.config.host, p.config.host);
        assert!(!copy.has_password);
        assert_eq!(s.get_profile(copy.config.id).unwrap(), copy);
        assert!(matches!(
            s.duplicate_profile(Uuid::new_v4()),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn deleting_group_moves_children_up() {
        let s = Store::open_in_memory().unwrap();
        let root = s.save_group(&group("root", None)).unwrap();
        let mid = s.save_group(&group("mid", Some(root.id))).unwrap();
        let leaf = s.save_group(&group("leaf", Some(mid.id))).unwrap();
        let mut p = profile("p");
        p.group_id = Some(mid.id);
        let p = s.save_profile(&p).unwrap();

        s.delete_group(mid.id).unwrap();
        let snap = s.snapshot().unwrap();
        assert_eq!(snap.groups.len(), 2);
        let leaf_now = snap.groups.iter().find(|g| g.id == leaf.id).unwrap();
        assert_eq!(leaf_now.parent_id, Some(root.id));
        assert_eq!(snap.profiles[0].group_id, Some(root.id));

        // Deleting a top-level group moves children to the top level.
        s.delete_group(root.id).unwrap();
        let snap = s.snapshot().unwrap();
        assert_eq!(snap.groups[0].parent_id, None);
        assert_eq!(s.get_profile(p.config.id).unwrap().group_id, None);
    }

    #[test]
    fn group_cycles_are_rejected() {
        let s = Store::open_in_memory().unwrap();
        let a = s.save_group(&group("a", None)).unwrap();
        let b = s.save_group(&group("b", Some(a.id))).unwrap();
        let c = s.save_group(&group("c", Some(b.id))).unwrap();
        let mut a2 = a.clone();
        a2.parent_id = Some(a.id);
        assert!(matches!(s.save_group(&a2), Err(Error::Storage(_))));
        a2.parent_id = Some(c.id);
        assert!(matches!(s.save_group(&a2), Err(Error::Storage(_))));
        // A legal move still works.
        let mut c2 = c.clone();
        c2.parent_id = Some(a.id);
        s.save_group(&c2).unwrap();
    }

    #[test]
    fn migrations_upgrade_old_database_without_data_loss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        // A database created by an older app version (schema v1 only).
        let p = {
            let conn = Connection::open(&path).unwrap();
            let s = Store::init(conn, &MIGRATIONS[..1]).unwrap();
            assert_eq!(s.schema_version().unwrap(), 1);
            let g = s.save_group(&group("old", None)).unwrap();
            let mut p = profile("legacy");
            p.group_id = Some(g.id);
            s.save_profile(&p).unwrap()
        };
        // A later version appends a migration; opening upgrades in place.
        let newer: Vec<&str> = MIGRATIONS
            .iter()
            .copied()
            .chain(["ALTER TABLE connections ADD COLUMN note TEXT;"])
            .collect();
        let s = Store::init(Connection::open(&path).unwrap(), &newer).unwrap();
        assert_eq!(s.schema_version().unwrap(), newer.len() as u32);
        assert_eq!(s.get_profile(p.config.id).unwrap(), p);
        assert_eq!(s.snapshot().unwrap().groups.len(), 1);
        s.lock()
            .execute("UPDATE connections SET note = 'x'", [])
            .unwrap();
        drop(s);

        // Reopening with the app's real migrations again is idempotent for this DB version
        // only when it is not newer; a downgrade must be refused instead of corrupting.
        assert!(matches!(
            Store::open(&path),
            Err(Error::Storage(m)) if m.contains("newer")
        ));
    }

    #[test]
    fn failed_migration_rolls_back() {
        let mut conn = Connection::open_in_memory().unwrap();
        let bad = ["CREATE TABLE a (x INTEGER);", "CREATE TABLE a (y INTEGER);"];
        assert!(migrate(&mut conn, &bad).is_err());
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
    }

    #[test]
    fn open_creates_parent_directory_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/db.sqlite");
        let id = {
            let s = Store::open(&path).unwrap();
            s.save_profile(&profile("persist")).unwrap().config.id
        };
        let s = Store::open(&path).unwrap();
        assert_eq!(s.get_profile(id).unwrap().config.name, "persist");
    }

    #[test]
    fn v1_database_upgrades_to_v2_keeping_profiles_and_gaining_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.sqlite");
        let p = {
            let s = Store::init(Connection::open(&path).unwrap(), &MIGRATIONS[..1]).unwrap();
            assert_eq!(s.schema_version().unwrap(), 1);
            s.save_profile(&profile("legacy")).unwrap()
        };
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), 2);
        assert_eq!(s.get_profile(p.config.id).unwrap(), p);
        assert_eq!(s.get_settings().unwrap(), AppSettings::default());
    }

    #[test]
    fn settings_roundtrip_normalize_and_tolerate_garbage() {
        let s = Store::open_in_memory().unwrap();
        let saved = s
            .set_settings(AppSettings {
                grid_max_rows: 5,
                log_level: crate::settings::LogLevel::Debug,
                ..AppSettings::default()
            })
            .unwrap();
        assert_eq!(saved.grid_max_rows, 1_000);
        assert_eq!(s.get_settings().unwrap(), saved);
        s.lock()
            .execute("UPDATE settings SET value = 'not json'", [])
            .unwrap();
        assert_eq!(s.get_settings().unwrap(), AppSettings::default());
        // One unusable field only resets that field.
        s.lock()
            .execute(
                "UPDATE settings SET value = ?1",
                params![r#"{"logLevel":"loud","gridMaxRows":2000,"timestampDisplay":"local","gridMaxCells":"x"}"#],
            )
            .unwrap();
        let got = s.get_settings().unwrap();
        assert_eq!(got.log_level, crate::settings::LogLevel::Info);
        assert_eq!(got.grid_max_rows, 2000);
        assert_eq!(got.grid_max_cells, AppSettings::default().grid_max_cells);
        assert_eq!(
            got.timestamp_display,
            crate::settings::TimestampDisplay::Local
        );
    }
}
