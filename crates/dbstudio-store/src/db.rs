//! SQLite persistence of connection groups and profiles (no secrets are stored here).

use std::collections::{BTreeMap, HashSet};
use std::fmt::Display;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use dbstudio_core::{AuthMode, ConnectionConfig, DbKind, Error, Result, SshConfig, TlsConfig};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::model::{ConnectionGroup, ConnectionProfile, ConnectionsSnapshot};
use crate::settings::AppSettings;
use crate::validate::{normalize_profile, validate_group, validate_profile};

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
    // v3: editor (QUERY-01, 07, 08)
    r##"CREATE TABLE query_history (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    connection_id  TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    database_name  TEXT,
    kind           TEXT NOT NULL,          -- query | explain | script | changes
    text           TEXT NOT NULL,          -- ≤ 100 000 chars; secrets redacted (see below)
    text_truncated INTEGER NOT NULL DEFAULT 0,
    started_at     TEXT NOT NULL,
    elapsed_ms     INTEGER,
    status         TEXT NOT NULL,          -- running | ok | error | cancelled
    statements     INTEGER,
    rows           INTEGER,
    error_message  TEXT
);
CREATE INDEX idx_history_conn_time ON query_history(connection_id, started_at DESC);
CREATE INDEX idx_history_time ON query_history(started_at DESC);
CREATE TABLE saved_query_folders (
    id         TEXT PRIMARY KEY NOT NULL,
    name       TEXT NOT NULL,
    parent_id  TEXT REFERENCES saved_query_folders(id) ON DELETE CASCADE,
    sort_order INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE saved_queries (
    id            TEXT PRIMARY KEY NOT NULL,
    folder_id     TEXT REFERENCES saved_query_folders(id) ON DELETE SET NULL,
    name          TEXT NOT NULL,
    text          TEXT NOT NULL,
    is_snippet    INTEGER NOT NULL DEFAULT 0,  -- snippet placeholders ${1:name}
    db_kind       TEXT,                        -- NULL = any
    connection_id TEXT REFERENCES connections(id) ON DELETE SET NULL,
    description   TEXT,
    sort_order    INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE INDEX idx_saved_folder ON saved_queries(folder_id);
CREATE TABLE editor_tabs (
    id            TEXT PRIMARY KEY NOT NULL,
    position      INTEGER NOT NULL,
    title         TEXT NOT NULL,
    connection_id TEXT REFERENCES connections(id) ON DELETE SET NULL,
    database_name TEXT,
    schema_name   TEXT,
    text          TEXT NOT NULL,
    file_path     TEXT,
    cursor        INTEGER NOT NULL DEFAULT 0,
    auto_commit   INTEGER NOT NULL DEFAULT 1,
    updated_at    TEXT NOT NULL
);"##,
    // v4: ERD + export presets (ERD-05, EXPORT-10)
    r##"CREATE TABLE erd_layouts (
    id            TEXT PRIMARY KEY NOT NULL,
    connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    scope         TEXT NOT NULL,   -- ObjectPath JSON
    tables        TEXT NOT NULL,   -- JSON [{path, x, y, collapsed, color?}]
    options       TEXT NOT NULL,   -- JSON {columns:"all"|"keys", showTypes, showComments, notation:"crowsFoot"}
    viewport      TEXT,            -- JSON {x, y, zoom}
    snapshot      TEXT NOT NULL,   -- JSON {tablePathKey: [[column, typeName], ...]} for change detection
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    UNIQUE(connection_id, name)
);
CREATE INDEX idx_erd_conn ON erd_layouts(connection_id);
CREATE TABLE export_presets (
    id            TEXT PRIMARY KEY NOT NULL,
    name          TEXT NOT NULL,
    connection_id TEXT REFERENCES connections(id) ON DELETE CASCADE,
    spec          TEXT NOT NULL,   -- ExportSpec JSON (destination path kept, re-granted on run)
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);"##,
    // v5: backup (BACKUP-07, 08, 11, 12)
    r##"CREATE TABLE backup_jobs (
    id                 TEXT PRIMARY KEY NOT NULL,
    name               TEXT NOT NULL,
    connection_id      TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    enabled            INTEGER NOT NULL DEFAULT 1,
    spec               TEXT NOT NULL,     -- BackupSpec JSON
    cron               TEXT,              -- NULL = manual only
    timezone           TEXT NOT NULL DEFAULT 'local',
    retention          TEXT NOT NULL,     -- Retention JSON
    missed_policy      TEXT NOT NULL DEFAULT 'runOnce',
    notify             TEXT NOT NULL,     -- NotifyPolicy JSON
    encrypted          INTEGER NOT NULL DEFAULT 0,  -- passphrase in keychain account "backup:<id>"
    last_scheduled_for TEXT,
    last_run_at        TEXT,
    next_run_at        TEXT,
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL
);
CREATE INDEX idx_backup_jobs_due ON backup_jobs(enabled, next_run_at);
CREATE TABLE backup_runs (
    id            TEXT PRIMARY KEY NOT NULL,
    job_id        TEXT REFERENCES backup_jobs(id) ON DELETE SET NULL,
    job_name      TEXT NOT NULL,
    connection_id TEXT REFERENCES connections(id) ON DELETE SET NULL,
    kind          TEXT NOT NULL,          -- backup | restore | verify
    trigger       TEXT NOT NULL,          -- manual | schedule | catchUp | cli
    engine        TEXT NOT NULL,
    status        TEXT NOT NULL,          -- running | succeeded | failed | cancelled | interrupted
    started_at    TEXT NOT NULL,
    finished_at   TEXT,
    duration_ms   INTEGER,
    file_path     TEXT,
    size_bytes    INTEGER,
    checksum      TEXT,
    objects       INTEGER,
    rows          INTEGER,
    error_kind    TEXT,
    error_message TEXT,
    log_tail      TEXT                    -- last 16 KB
);
CREATE INDEX idx_backup_runs_job ON backup_runs(job_id, started_at DESC);
CREATE INDEX idx_backup_runs_time ON backup_runs(started_at DESC);"##,
    // v6: SSH (CONN-06)
    r##"ALTER TABLE connections ADD COLUMN ssh TEXT;   -- SshConfig JSON or NULL
CREATE TABLE ssh_known_hosts (
    host        TEXT NOT NULL,
    port        INTEGER NOT NULL,
    key_type    TEXT NOT NULL,
    fingerprint TEXT NOT NULL,              -- SHA256:base64
    added_at    TEXT NOT NULL,
    PRIMARY KEY (host, port, key_type)
);"##,
    // v7: environment label, editor dirty flag, plan history (CONN-03, PLAN-07)
    r##"ALTER TABLE connections ADD COLUMN environment_label TEXT;   -- custom label, NULL = standard name
ALTER TABLE editor_tabs ADD COLUMN dirty INTEGER NOT NULL DEFAULT 0;
CREATE TABLE plan_history (
    id            TEXT PRIMARY KEY NOT NULL,
    connection_id TEXT REFERENCES connections(id) ON DELETE CASCADE,
    created_at    TEXT NOT NULL,
    database_name TEXT,
    statement     TEXT NOT NULL,          -- redacted with dbstudio_sql::redact
    analyze       INTEGER NOT NULL,
    format        TEXT NOT NULL,          -- PlanFormat
    raw_plan      TEXT NOT NULL,          -- RawPlan JSON (<= 8 MiB, bigger plans are not stored)
    total_cost    REAL,
    total_ms      REAL,
    warnings      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_plan_history_conn ON plan_history(connection_id, created_at DESC);"##,
    // v8: SQL debugger. Per-tab breakpoints/watches and routine breakpoints per connection.
    r##"ALTER TABLE editor_tabs ADD COLUMN debug_json TEXT;          -- NULL = nothing saved
CREATE TABLE routine_breakpoints (
    id            TEXT NOT NULL,               -- Breakpoint.id (unique per connection)
    connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    signature     TEXT NOT NULL,
    source_hash   TEXT,
    body_json     TEXT NOT NULL,               -- RoutineBreakpoint (JSON: routine + sourceHash + breakpoint)
    updated_at    TEXT NOT NULL,
    PRIMARY KEY (connection_id, id)
);
CREATE INDEX routine_breakpoints_conn ON routine_breakpoints(connection_id);"##,
    // v9: authentication mode of a connection (`password` | `none`).
    r##"ALTER TABLE connections ADD COLUMN auth_mode TEXT NOT NULL DEFAULT 'password';"##,
];

const PROFILE_COLUMNS: &str = "id, name, kind, host, port, database_name, username, tls_mode, \
    tls_ca_cert_path, tls_client_cert_path, tls_client_key_path, connect_timeout_ms, read_only, \
    environment, options, group_id, color, save_password, has_password, sort_order, created_at, \
    updated_at, ssh, environment_label, auth_mode";

pub(crate) fn db_err(e: impl Display) -> Error {
    Error::Storage(format!("database error: {e}"))
}

pub(crate) fn invalid(msg: impl Display) -> Error {
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

pub(crate) fn to_text<T: Serialize>(value: &T) -> Result<String> {
    match serde_json::to_value(value).map_err(invalid)? {
        serde_json::Value::String(s) => Ok(s),
        other => Err(invalid(format!("expected a string, got {other}"))),
    }
}

pub(crate) fn from_text<T: DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(text.to_string()))
        .map_err(|e| invalid(format!("unknown value `{text}`: {e}")))
}

pub(crate) fn parse_uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text).map_err(|e| invalid(format!("bad uuid `{text}`: {e}")))
}

pub(crate) fn opt_uuid(text: Option<String>) -> Result<Option<Uuid>> {
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
    let ssh: Option<String> = row.get("ssh")?;
    let environment_label: Option<String> = row.get("environment_label")?;
    let auth_mode: String = row.get("auth_mode")?;
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
                auth_mode: from_text::<AuthMode>(&auth_mode)?,
                tls: TlsConfig {
                    mode: from_text(&tls_mode)?,
                    ca_cert_path: ca,
                    client_cert_path: cert,
                    client_key_path: key,
                    server_name: None,
                },
                connect_timeout_ms: timeout.map(|t| t.max(0) as u64),
                read_only,
                environment: environment.as_deref().map(from_text).transpose()?,
                options: serde_json::from_str::<BTreeMap<String, String>>(&options)
                    .map_err(|e| invalid(format!("bad options json: {e}")))?,
                ssh: ssh
                    .as_deref()
                    .map(serde_json::from_str::<SshConfig>)
                    .transpose()
                    .map_err(|e| invalid(format!("bad ssh json: {e}")))?,
                environment_label,
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

    pub(crate) fn lock(&self) -> MutexGuard<'_, Connection> {
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
        normalize_profile(&mut profile);
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
            return Err(Error::Invalid(
                "a group cannot be moved into itself or its descendants".into(),
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
    let ssh = c
        .ssh
        .as_ref()
        .map(|s| serde_json::to_string(s).map_err(invalid))
        .transpose()?;
    tx.execute(
        &format!(
            "INSERT INTO connections ({PROFILE_COLUMNS}) VALUES
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
              ?19, ?20, ?21, ?22, ?23, ?24, ?25)
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
                updated_at = excluded.updated_at, ssh = excluded.ssh,
                environment_label = excluded.environment_label,
                auth_mode = excluded.auth_mode"
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
            ssh,
            c.environment_label,
            to_text(&c.auth_mode)?,
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{Environment, SshAuth, TlsMode};

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
            server_name: None,
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
        assert!(matches!(s.save_profile(&p), Err(Error::Invalid(_))));
        p.config.name = "ok".into();
        p.config.host = " ".into();
        assert!(matches!(s.save_profile(&p), Err(Error::Invalid(_))));
        p.config.host = "h".into();
        p.config.port = 0;
        assert!(matches!(s.save_profile(&p), Err(Error::Invalid(_))));
        p.config.port = 1;
        p.group_id = Some(Uuid::new_v4());
        assert!(matches!(s.save_profile(&p), Err(Error::NotFound(_))));
        p.group_id = None;
        s.save_profile(&p).unwrap();
        assert!(matches!(
            s.save_group(&group("", None)),
            Err(Error::Invalid(_))
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
        assert!(matches!(s.save_group(&a2), Err(Error::Invalid(_))));
        a2.parent_id = Some(c.id);
        assert!(matches!(s.save_group(&a2), Err(Error::Invalid(_))));
        // A legal move still works.
        let mut c2 = c.clone();
        c2.parent_id = Some(a.id);
        s.save_group(&c2).unwrap();
    }

    /// Insert a connection into a database at schema v1/v2 (no `ssh` column yet).
    fn insert_legacy(conn: &Connection, name: &str, group: Option<Uuid>) -> Uuid {
        let id = Uuid::new_v4();
        conn.execute(
            "INSERT INTO connections (id, name, kind, host, port, tls_mode, group_id, created_at, \
             updated_at) VALUES (?1, ?2, 'postgres', 'localhost', 5432, 'prefer', ?3, \
             '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
            params![id.to_string(), name, group.map(|g| g.to_string())],
        )
        .unwrap();
        id
    }

    #[test]
    fn migrations_upgrade_old_database_without_data_loss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        // A database created by an older app version (schema v1 only).
        let (gid, pid) = {
            let conn = Connection::open(&path).unwrap();
            let s = Store::init(conn, &MIGRATIONS[..1]).unwrap();
            assert_eq!(s.schema_version().unwrap(), 1);
            let g = s.save_group(&group("old", None)).unwrap();
            let pid = insert_legacy(&s.lock(), "legacy", Some(g.id));
            (g.id, pid)
        };
        // A later version appends a migration; opening upgrades in place.
        let newer: Vec<&str> = MIGRATIONS
            .iter()
            .copied()
            .chain(["ALTER TABLE connections ADD COLUMN note TEXT;"])
            .collect();
        let s = Store::init(Connection::open(&path).unwrap(), &newer).unwrap();
        assert_eq!(s.schema_version().unwrap(), newer.len() as u32);
        let p = s.get_profile(pid).unwrap();
        assert_eq!(p.config.name, "legacy");
        assert_eq!(p.group_id, Some(gid));
        assert_eq!(p.config.ssh, None);
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

    fn table_exists(s: &Store, name: &str) -> bool {
        s.lock()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                params![name],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn v1_database_upgrades_to_latest_keeping_profiles_and_gaining_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.sqlite");
        let pid = {
            let s = Store::init(Connection::open(&path).unwrap(), &MIGRATIONS[..1]).unwrap();
            assert_eq!(s.schema_version().unwrap(), 1);
            insert_legacy(&s.lock(), "legacy", None)
        };
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), 9);
        assert_eq!(s.get_profile(pid).unwrap().config.name, "legacy");
        assert_eq!(s.get_settings().unwrap(), AppSettings::default());
    }

    #[test]
    fn v2_database_with_data_upgrades_to_latest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v2.sqlite");
        let (gid, pid, settings) = {
            let s = Store::init(Connection::open(&path).unwrap(), &MIGRATIONS[..2]).unwrap();
            assert_eq!(s.schema_version().unwrap(), 2);
            assert!(!table_exists(&s, "query_history"));
            let g = s.save_group(&group("g", None)).unwrap();
            let pid = insert_legacy(&s.lock(), "old-conn", Some(g.id));
            let settings = s
                .set_settings(AppSettings {
                    grid_max_rows: 2_000,
                    log_level: crate::settings::LogLevel::Debug,
                    ..AppSettings::default()
                })
                .unwrap();
            (g.id, pid, settings)
        };
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), 9);
        assert_eq!(MIGRATIONS.len(), 9);
        for t in [
            "query_history",
            "saved_query_folders",
            "saved_queries",
            "editor_tabs",
            "erd_layouts",
            "export_presets",
            "backup_jobs",
            "backup_runs",
            "ssh_known_hosts",
            "plan_history",
        ] {
            assert!(table_exists(&s, t), "missing table {t}");
        }
        for (table, column) in [
            ("connections", "environment_label"),
            ("editor_tabs", "dirty"),
        ] {
            let has: bool = s
                .lock()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
                    params![table, column],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(has, "missing column {table}.{column}");
        }
        let has_ssh: bool = s
            .lock()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('connections') WHERE name = 'ssh')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(has_ssh);
        let p = s.get_profile(pid).unwrap();
        assert_eq!(p.config.name, "old-conn");
        assert_eq!(p.group_id, Some(gid));
        assert_eq!(p.config.ssh, None);
        assert_eq!(s.snapshot().unwrap().groups.len(), 1);
        let got = s.get_settings().unwrap();
        assert_eq!(got.grid_max_rows, 2_000);
        assert_eq!(got.log_level, settings.log_level);
        // New settings fields take their defaults.
        assert_eq!(got.history_max_per_connection, 5_000);
        drop(s);
        // Reopening is idempotent.
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), 9);
        assert_eq!(s.get_profile(pid).unwrap().config.name, "old-conn");
    }

    #[test]
    fn environment_label_and_new_kinds_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        let mut p = profile("labelled");
        p.config.kind = DbKind::CockroachDb;
        p.config.environment_label = Some("QA-3".into());
        let saved = s.save_profile(&p).unwrap();
        let loaded = s.get_profile(saved.config.id).unwrap();
        assert_eq!(loaded.config.environment_label.as_deref(), Some("QA-3"));
        assert_eq!(loaded.config.kind, DbKind::CockroachDb);
        let mut edit = loaded;
        edit.config.environment_label = None;
        s.save_profile(&edit).unwrap();
        assert_eq!(
            s.get_profile(saved.config.id)
                .unwrap()
                .config
                .environment_label,
            None
        );
    }

    #[test]
    fn environment_label_is_trimmed_and_limited() {
        let s = Store::open_in_memory().unwrap();
        let mut p = profile("lbl");
        p.config.environment_label = Some("  QA-3  ".into());
        let saved = s.save_profile(&p).unwrap();
        assert_eq!(saved.config.environment_label.as_deref(), Some("QA-3"));
        p.config.environment_label = Some("   ".into());
        let saved = s.save_profile(&p).unwrap();
        assert_eq!(saved.config.environment_label, None);
        p.config.environment_label = Some("x".repeat(32));
        s.save_profile(&p).unwrap();
        p.config.environment_label = Some("x".repeat(33));
        assert!(matches!(s.save_profile(&p), Err(Error::Invalid(_))));
        p.config.environment_label = Some("a\nb".into());
        assert!(matches!(s.save_profile(&p), Err(Error::Invalid(_))));
    }

    #[test]
    fn ssh_config_roundtrips_and_null_when_absent() {
        let s = Store::open_in_memory().unwrap();
        let mut p = profile("tunnelled");
        p.config.ssh = Some(SshConfig {
            host: "bastion".into(),
            port: 2222,
            username: "ops".into(),
            auth: SshAuth::PrivateKey {
                key_path: "/home/u/.ssh/id_ed25519".into(),
                has_passphrase: true,
            },
            keepalive_secs: Some(30),
            connect_timeout_ms: Some(5_000),
        });
        let saved = s.save_profile(&p).unwrap();
        let loaded = s.get_profile(p.config.id).unwrap();
        assert_eq!(loaded, saved);
        assert_eq!(loaded.config.ssh, p.config.ssh);
        let raw: String = s
            .lock()
            .query_row("SELECT ssh FROM connections", [], |r| r.get(0))
            .unwrap();
        assert!(raw.contains("\"method\":\"privateKey\""));

        // Other auth methods and clearing back to NULL.
        for auth in [SshAuth::Password, SshAuth::Agent] {
            let mut edit = loaded.clone();
            edit.config.ssh.as_mut().unwrap().auth = auth.clone();
            s.save_profile(&edit).unwrap();
            assert_eq!(
                s.get_profile(p.config.id).unwrap().config.ssh.unwrap().auth,
                auth
            );
        }
        let mut edit = loaded;
        edit.config.ssh = None;
        s.save_profile(&edit).unwrap();
        assert_eq!(s.get_profile(p.config.id).unwrap().config.ssh, None);
        let null: bool = s
            .lock()
            .query_row("SELECT ssh IS NULL FROM connections", [], |r| r.get(0))
            .unwrap();
        assert!(null);
        // Duplicate keeps the tunnel configuration.
        let mut with = edit;
        with.config.ssh = p.config.ssh.clone();
        s.save_profile(&with).unwrap();
        let copy = s.duplicate_profile(p.config.id).unwrap();
        assert_eq!(copy.config.ssh, p.config.ssh);
    }

    #[test]
    fn deleting_a_connection_cascades_and_nulls_dependents() {
        let s = Store::open_in_memory().unwrap();
        let p = s.save_profile(&profile("c")).unwrap();
        let id = p.config.id.to_string();
        let conn = s.lock();
        conn.execute(
            "INSERT INTO query_history (connection_id, kind, text, started_at, status)
             VALUES (?1, 'query', 'select 1', '2024-01-01T00:00:00Z', 'ok')",
            params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO saved_queries (id, name, text, connection_id, created_at, updated_at)
             VALUES ('q1', 'n', 't', ?1, 'x', 'x')",
            params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO editor_tabs (id, position, title, connection_id, text, updated_at)
             VALUES ('00000000-0000-0000-0000-0000000000a1', 0, 'T', ?1, '', 'x')",
            params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO erd_layouts (id, connection_id, name, scope, tables, options, snapshot,
             created_at, updated_at) VALUES ('e1', ?1, 'n', '{}', '[]', '{}', '{}', 'x', 'x')",
            params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO export_presets (id, name, connection_id, spec, created_at, updated_at)
             VALUES ('p1', 'n', ?1, '{}', 'x', 'x')",
            params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO backup_jobs (id, name, connection_id, spec, retention, notify,
             created_at, updated_at) VALUES ('j1', 'n', ?1, '{}', '{}', '{}', 'x', 'x')",
            params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO backup_runs (id, job_id, job_name, connection_id, kind, trigger, engine,
             status, started_at) VALUES ('r1', 'j1', 'n', ?1, 'backup', 'manual', 'native',
             'succeeded', 'x')",
            params![id],
        )
        .unwrap();
        drop(conn);

        s.delete_profile(p.config.id).unwrap();

        let conn = s.lock();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("SELECT COUNT(*) FROM query_history"), 0);
        assert_eq!(count("SELECT COUNT(*) FROM erd_layouts"), 0);
        assert_eq!(count("SELECT COUNT(*) FROM export_presets"), 0);
        assert_eq!(count("SELECT COUNT(*) FROM backup_jobs"), 0);
        // Saved queries, tabs and run records survive without the link.
        assert_eq!(
            count("SELECT COUNT(*) FROM saved_queries WHERE connection_id IS NULL"),
            1
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM editor_tabs WHERE connection_id IS NULL"),
            1
        );
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM backup_runs WHERE connection_id IS NULL AND job_id IS NULL"
            ),
            1
        );
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

    #[test]
    fn migration_v8_upgrades_a_v7_database_with_tabs() {
        assert_eq!(MIGRATIONS.len(), 9);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v7.sqlite");
        {
            let s = Store::init(Connection::open(&path).unwrap(), &MIGRATIONS[..7]).unwrap();
            assert_eq!(s.schema_version().unwrap(), 7);
            s.lock()
                .execute(
                    "INSERT INTO editor_tabs (id, position, title, text, cursor, auto_commit, dirty, updated_at)
                     VALUES ('00000000-0000-0000-0000-0000000000a1', 0, 'Query', 'select 1', 0, 1, 0, 'now')",
                    [],
                )
                .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), 9);
        let tabs = s.tabs_get().unwrap();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].debug, None);
        let conn = s.lock();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM routine_breakpoints", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
}
