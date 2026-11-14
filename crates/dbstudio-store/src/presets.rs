//! Saved export presets (`export_presets`). Package G owns this file.

use dbstudio_core::{Error, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use rusqlite::{OptionalExtension, params};

use crate::db::{Store, db_err, invalid, now_iso, opt_uuid, parse_uuid};

/// A named export configuration. `spec` is an `ExportSpec` JSON document (opaque here).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportPreset {
    /// Preset id.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Connection the preset belongs to; `None` = usable anywhere.
    pub connection_id: Option<Uuid>,
    /// The export specification.
    pub spec: serde_json::Value,
    /// ISO-8601 creation time.
    pub created_at: String,
    /// ISO-8601 last update time.
    pub updated_at: String,
}

const RECENT_DIRS_KEY: &str = "export.recentDirs";
/// Most directories remembered in `export.recentDirs`.
pub const RECENT_DIRS_MAX: usize = 20;

const COLUMNS: &str = "id, name, connection_id, spec, created_at, updated_at";

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<ExportPreset>> {
    let id: String = row.get(0)?;
    let name: String = row.get(1)?;
    let connection_id: Option<String> = row.get(2)?;
    let spec: String = row.get(3)?;
    let created_at: String = row.get(4)?;
    let updated_at: String = row.get(5)?;
    Ok((|| {
        Ok(ExportPreset {
            id: parse_uuid(&id)?,
            name,
            connection_id: opt_uuid(connection_id)?,
            spec: serde_json::from_str(&spec).map_err(invalid)?,
            created_at,
            updated_at,
        })
    })())
}

impl Store {
    /// Presets of a connection plus the global ones (or all when `connection_id` is `None`),
    /// ordered by name.
    pub fn presets_list(&self, connection_id: Option<Uuid>) -> Result<Vec<ExportPreset>> {
        let conn = self.lock();
        let (sql, arg): (String, Option<String>) = match connection_id {
            Some(id) => (
                format!(
                    "SELECT {COLUMNS} FROM export_presets \
                     WHERE connection_id = ?1 OR connection_id IS NULL \
                     ORDER BY name COLLATE NOCASE, id"
                ),
                Some(id.to_string()),
            ),
            None => (
                format!("SELECT {COLUMNS} FROM export_presets ORDER BY name COLLATE NOCASE, id"),
                None,
            ),
        };
        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let mut rows = match &arg {
            Some(a) => stmt.query(params![a]),
            None => stmt.query([]),
        }
        .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(db_err)? {
            out.push(from_row(row).map_err(db_err)??);
        }
        Ok(out)
    }

    /// Insert or update (by id). The creation time of an existing preset is kept and
    /// `updated_at` is set to now.
    pub fn presets_save(&self, preset: &ExportPreset) -> Result<ExportPreset> {
        let name = preset.name.trim();
        if name.is_empty() {
            return Err(Error::Invalid("preset name is empty".into()));
        }
        let now = now_iso();
        let conn = self.lock();
        let existing: Option<String> = conn
            .query_row(
                "SELECT created_at FROM export_presets WHERE id = ?1",
                params![preset.id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        let created_at = existing.clone().unwrap_or_else(|| {
            if preset.created_at.is_empty() {
                now.clone()
            } else {
                preset.created_at.clone()
            }
        });
        let args = params![
            preset.id.to_string(),
            name,
            preset.connection_id.map(|c| c.to_string()),
            serde_json::to_string(&preset.spec).map_err(invalid)?,
            created_at,
            now,
        ];
        let result = if existing.is_some() {
            conn.execute(
                "UPDATE export_presets SET name = ?2, connection_id = ?3, spec = ?4, \
                 created_at = ?5, updated_at = ?6 WHERE id = ?1",
                args,
            )
        } else {
            conn.execute(
                "INSERT INTO export_presets (id, name, connection_id, spec, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                args,
            )
        };
        result.map_err(db_err)?;
        Ok(ExportPreset {
            id: preset.id,
            name: name.to_string(),
            connection_id: preset.connection_id,
            spec: preset.spec.clone(),
            created_at,
            updated_at: now,
        })
    }

    /// Directories recent exports were written to (`export.recentDirs` setting), most recent
    /// first. Unreadable data gives an empty list.
    pub fn export_recent_dirs(&self) -> Result<Vec<String>> {
        let text: Option<String> = self
            .lock()
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![RECENT_DIRS_KEY],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        Ok(text
            .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
            .unwrap_or_default())
    }

    /// Remember `dir` as the most recent export directory (deduplicated, at most
    /// [`RECENT_DIRS_MAX`]). Directories that no longer exist are dropped from the list.
    pub fn export_recent_dirs_record(&self, dir: &str) -> Result<Vec<String>> {
        if dir.trim().is_empty() {
            return Err(Error::Invalid("export directory is empty".into()));
        }
        let mut list: Vec<String> = vec![dir.to_string()];
        for d in self.export_recent_dirs()? {
            if d != dir && std::path::Path::new(&d).is_dir() {
                list.push(d);
            }
        }
        list.truncate(RECENT_DIRS_MAX);
        let json = serde_json::to_string(&list).map_err(invalid)?;
        self.lock()
            .execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![RECENT_DIRS_KEY, json],
            )
            .map_err(db_err)?;
        Ok(list)
    }

    /// Delete a preset; an unknown id is `NotFound`.
    pub fn presets_delete(&self, id: Uuid) -> Result<()> {
        let n = self
            .lock()
            .execute(
                "DELETE FROM export_presets WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("export preset {id}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn preset(name: &str, connection_id: Option<Uuid>) -> ExportPreset {
        ExportPreset {
            id: Uuid::new_v4(),
            name: name.into(),
            connection_id,
            spec: json!({"format": {"kind": "csv"}}),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn connection(store: &Store) -> Uuid {
        let id = Uuid::new_v4();
        store
            .lock()
            .execute(
                "INSERT INTO connections (id, name, kind, host, port, tls_mode, created_at, updated_at) \
                 VALUES (?1, 'c', 'postgres', 'h', 1, 'disable', 'x', 'x')",
                params![id.to_string()],
            )
            .unwrap();
        id
    }

    #[test]
    fn recent_dirs_are_deduplicated_capped_and_pruned() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.export_recent_dirs().unwrap().is_empty());
        let root = std::env::temp_dir();
        let a = root.to_string_lossy().into_owned();
        let gone = root.join("dbstudio-definitely-missing-dir");
        let gone = gone.to_string_lossy().into_owned();
        store.export_recent_dirs_record(&gone).unwrap();
        assert_eq!(store.export_recent_dirs().unwrap(), [gone.clone()]);
        // Recording another directory drops the one that no longer exists.
        let list = store.export_recent_dirs_record(&a).unwrap();
        assert_eq!(list, [a.clone()]);
        let list = store.export_recent_dirs_record(&a).unwrap();
        assert_eq!(list, [a.clone()]);
        assert!(store.export_recent_dirs_record(" ").is_err());
        // Cap.
        let dirs: Vec<tempfile::TempDir> = (0..25).map(|_| tempfile::tempdir().unwrap()).collect();
        for d in &dirs {
            store
                .export_recent_dirs_record(&d.path().to_string_lossy())
                .unwrap();
        }
        let list = store.export_recent_dirs().unwrap();
        assert_eq!(list.len(), RECENT_DIRS_MAX);
        assert_eq!(list[0], dirs[24].path().to_string_lossy());
    }

    #[test]
    fn save_list_update_delete() {
        let store = Store::open_in_memory().unwrap();
        let conn_a = connection(&store);
        let conn_b = connection(&store);
        let a = store.presets_save(&preset("Bravo", Some(conn_a))).unwrap();
        store.presets_save(&preset("alpha", None)).unwrap();
        store.presets_save(&preset("Other", Some(conn_b))).unwrap();
        assert!(!a.created_at.is_empty());

        let names = |v: Vec<ExportPreset>| v.into_iter().map(|p| p.name).collect::<Vec<_>>();
        assert_eq!(
            names(store.presets_list(Some(conn_a)).unwrap()),
            ["alpha", "Bravo"]
        );
        assert_eq!(store.presets_list(None).unwrap().len(), 3);

        let mut edited = a.clone();
        edited.name = "  Renamed ".into();
        edited.spec = json!({"x": 1});
        let saved = store.presets_save(&edited).unwrap();
        assert_eq!(saved.name, "Renamed");
        assert_eq!(saved.created_at, a.created_at);
        let again = store.presets_list(Some(conn_a)).unwrap();
        assert!(
            again
                .iter()
                .any(|p| p.id == a.id && p.spec == json!({"x": 1}))
        );

        store.presets_delete(a.id).unwrap();
        assert!(matches!(
            store.presets_delete(a.id),
            Err(Error::NotFound(_))
        ));
        assert!(
            store
                .presets_save(&preset("  ", None))
                .is_err_and(|e| e.kind() == "invalid")
        );
    }
}
