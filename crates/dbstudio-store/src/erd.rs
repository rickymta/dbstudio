//! Saved ERD layouts (`erd_layouts`). Package F owns this file.

use dbstudio_core::{Error, ObjectPath, Result};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{Store, db_err, now_iso, parse_uuid};

/// Prefix of the error message of a save that lost an optimistic-concurrency check.
pub const ERD_CONFLICT: &str = "layout-conflict";

/// A saved diagram layout. The JSON columns are opaque to the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErdLayout {
    /// Layout id.
    pub id: Uuid,
    /// Owning connection.
    pub connection_id: Uuid,
    /// Name, unique per connection.
    pub name: String,
    /// The schema/database the diagram covers.
    pub scope: ObjectPath,
    /// `[{path, x, y, collapsed, color?}]`.
    pub tables: serde_json::Value,
    /// `{columns, showTypes, showComments, notation}`.
    pub options: serde_json::Value,
    /// `{x, y, zoom}`.
    #[serde(default)]
    pub viewport: Option<serde_json::Value>,
    /// `{tablePathKey: [[column, typeName], ...]}` used to detect schema changes.
    pub snapshot: serde_json::Value,
    /// ISO-8601 creation time.
    pub created_at: String,
    /// ISO-8601 last update time.
    pub updated_at: String,
}

/// Listing entry of a saved layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErdLayoutSummary {
    /// Layout id.
    pub id: Uuid,
    /// Layout name.
    pub name: String,
    /// The schema/database the diagram covers.
    pub scope: ObjectPath,
    /// Number of tables in the layout.
    pub table_count: u32,
    /// ISO-8601 last update time.
    pub updated_at: String,
}

fn to_json<T: Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| Error::Storage(format!("invalid data: {e}")))
}

fn from_json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|e| Error::Storage(format!("invalid data: {e}")))
}

const COLUMNS: &str =
    "id, connection_id, name, scope, tables, options, viewport, snapshot, created_at, updated_at";

fn layout_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<ErdLayout>> {
    let id: String = row.get(0)?;
    let connection_id: String = row.get(1)?;
    let name: String = row.get(2)?;
    let scope: String = row.get(3)?;
    let tables: String = row.get(4)?;
    let options: String = row.get(5)?;
    let viewport: Option<String> = row.get(6)?;
    let snapshot: String = row.get(7)?;
    let created_at: String = row.get(8)?;
    let updated_at: String = row.get(9)?;
    Ok((|| {
        Ok(ErdLayout {
            id: parse_uuid(&id)?,
            connection_id: parse_uuid(&connection_id)?,
            name,
            scope: from_json(&scope)?,
            tables: from_json(&tables)?,
            options: from_json(&options)?,
            viewport: viewport.as_deref().map(from_json).transpose()?,
            snapshot: from_json(&snapshot)?,
            created_at,
            updated_at,
        })
    })())
}

fn table_count(tables_json: &str) -> u32 {
    serde_json::from_str::<serde_json::Value>(tables_json)
        .ok()
        .and_then(|v| v.as_array().map(|a| a.len() as u32))
        .unwrap_or(0)
}

impl Store {
    /// Summaries of a connection's layouts, most recently updated first.
    pub fn erd_list(&self, connection_id: Uuid) -> Result<Vec<ErdLayoutSummary>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, scope, tables, updated_at FROM erd_layouts \
                 WHERE connection_id = ?1 ORDER BY updated_at DESC, name",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![connection_id.to_string()], |r| {
                let id: String = r.get(0)?;
                let name: String = r.get(1)?;
                let scope: String = r.get(2)?;
                let tables: String = r.get(3)?;
                let updated_at: String = r.get(4)?;
                Ok((id, name, scope, tables, updated_at))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, scope, tables, updated_at) = row.map_err(db_err)?;
            out.push(ErdLayoutSummary {
                id: parse_uuid(&id)?,
                name,
                scope: from_json(&scope)?,
                table_count: table_count(&tables),
                updated_at,
            });
        }
        Ok(out)
    }

    /// One layout or `NotFound`.
    pub fn erd_get(&self, id: Uuid) -> Result<ErdLayout> {
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {COLUMNS} FROM erd_layouts WHERE id = ?1"),
            params![id.to_string()],
            layout_from_row,
        )
        .optional()
        .map_err(db_err)?
        .ok_or_else(|| Error::NotFound(format!("ERD layout {id}")))?
    }

    /// Insert or update (by id); a duplicate name on the connection is an error. The creation
    /// time of an existing layout is kept, `updated_at` is set to now. Updating requires the
    /// `updated_at` of the version being replaced, else the error message starts with
    /// [`ERD_CONFLICT`].
    pub fn erd_save(&self, layout: &ErdLayout) -> Result<ErdLayout> {
        let name = layout.name.trim();
        if name.is_empty() {
            return Err(Error::Storage("invalid data: layout name is empty".into()));
        }
        let now = now_iso();
        let conn = self.lock();
        let existing: Option<(String, String)> = conn
            .query_row(
                "SELECT created_at, updated_at FROM erd_layouts WHERE id = ?1",
                params![layout.id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db_err)?;
        // Optimistic concurrency: the caller must have read the version it overwrites.
        if let Some((_, updated)) = &existing {
            if *updated != layout.updated_at {
                return Err(Error::Other(format!(
                    "{ERD_CONFLICT}: the layout was changed elsewhere since it was loaded"
                )));
            }
        }
        let existing_created = existing.map(|(created, _)| created);
        let created_at = existing_created.clone().unwrap_or_else(|| {
            if layout.created_at.is_empty() {
                now.clone()
            } else {
                layout.created_at.clone()
            }
        });
        let args = params![
            layout.id.to_string(),
            layout.connection_id.to_string(),
            name,
            to_json(&layout.scope)?,
            to_json(&layout.tables)?,
            to_json(&layout.options)?,
            layout.viewport.as_ref().map(to_json).transpose()?,
            to_json(&layout.snapshot)?,
            created_at,
            now,
        ];
        let result = if existing_created.is_some() {
            conn.execute(
                "UPDATE erd_layouts SET connection_id = ?2, name = ?3, scope = ?4, tables = ?5, \
                 options = ?6, viewport = ?7, snapshot = ?8, created_at = ?9, updated_at = ?10 \
                 WHERE id = ?1",
                args,
            )
        } else {
            conn.execute(
                "INSERT INTO erd_layouts (id, connection_id, name, scope, tables, options, \
                 viewport, snapshot, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                args,
            )
        };
        result.map_err(|e| match &e {
            rusqlite::Error::SqliteFailure(f, msg)
                if f.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                if msg.as_deref().is_some_and(|m| m.contains("UNIQUE")) {
                    Error::Other(format!("a layout named `{name}` already exists"))
                } else {
                    Error::NotFound(format!("connection {}", layout.connection_id))
                }
            }
            _ => db_err(e),
        })?;
        Ok(ErdLayout {
            name: name.to_string(),
            created_at,
            updated_at: now,
            ..layout.clone()
        })
    }

    /// Delete a layout (`NotFound` when it does not exist).
    pub fn erd_delete(&self, id: Uuid) -> Result<()> {
        let n = self
            .lock()
            .execute(
                "DELETE FROM erd_layouts WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("ERD layout {id}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use dbstudio_core::{ConnectionConfig, DbKind, ObjectKind};
    use serde_json::json;

    use super::*;
    use crate::model::ConnectionProfile;

    fn conn(s: &Store, name: &str) -> Uuid {
        s.save_profile(&ConnectionProfile {
            config: ConnectionConfig::new(DbKind::Postgres, name, "localhost"),
            group_id: None,
            color: None,
            save_password: false,
            has_password: false,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap()
        .config
        .id
    }

    fn layout(connection_id: Uuid, name: &str) -> ErdLayout {
        ErdLayout {
            id: Uuid::new_v4(),
            connection_id,
            name: name.into(),
            scope: ObjectPath::root().child(ObjectKind::Database, "app"),
            tables: json!([{"path": {"segments": []}, "x": 10.5, "y": -20.0, "collapsed": false}]),
            options: json!({"columns": "all", "showTypes": true, "notation": "crowsFoot"}),
            viewport: Some(json!({"x": 1.0, "y": 2.0, "zoom": 0.5})),
            snapshot: json!({"public/film": [["film_id", "integer"]]}),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn save_get_list_delete() {
        let s = Store::open_in_memory().unwrap();
        let c = conn(&s, "pg");
        let saved = s.erd_save(&layout(c, "Main")).unwrap();
        assert!(!saved.created_at.is_empty() && !saved.updated_at.is_empty());
        let got = s.erd_get(saved.id).unwrap();
        assert_eq!(got, saved);
        assert_eq!(got.tables[0]["x"], 10.5);
        assert_eq!(got.viewport.as_ref().unwrap()["zoom"], 0.5);

        let list = s.erd_list(c).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Main");
        assert_eq!(list[0].table_count, 1);
        assert!(s.erd_list(conn(&s, "other")).unwrap().is_empty());

        s.erd_delete(saved.id).unwrap();
        assert!(matches!(s.erd_get(saved.id), Err(Error::NotFound(_))));
        assert!(matches!(s.erd_delete(saved.id), Err(Error::NotFound(_))));
    }

    #[test]
    fn update_keeps_created_at_and_replaces_content() {
        let s = Store::open_in_memory().unwrap();
        let c = conn(&s, "pg");
        let first = s.erd_save(&layout(c, "Main")).unwrap();
        let mut edited = first.clone();
        edited.tables = json!([]);
        edited.viewport = None;
        edited.name = "Renamed".into();
        edited.created_at = "tampered".into();
        let second = s.erd_save(&edited).unwrap();
        assert_ne!(second.updated_at, String::new());
        assert_eq!(second.created_at, first.created_at);
        let got = s.erd_get(first.id).unwrap();
        assert_eq!(got.name, "Renamed");
        assert!(got.viewport.is_none());
        assert_eq!(s.erd_list(c).unwrap()[0].table_count, 0);
        assert_eq!(s.erd_list(c).unwrap().len(), 1);
    }

    #[test]
    fn stale_updates_are_refused() {
        let s = Store::open_in_memory().unwrap();
        let c = conn(&s, "pg");
        let first = s.erd_save(&layout(c, "Main")).unwrap();
        let mut stale = first.clone();
        stale.updated_at = "2000-01-01T00:00:00Z".into();
        let err = s.erd_save(&stale).unwrap_err();
        assert!(
            matches!(&err, Error::Other(m) if m.starts_with(ERD_CONFLICT)),
            "{err:?}"
        );
        // The current version is accepted and yields a new version.
        let second = s.erd_save(&first).unwrap();
        assert_ne!(second.updated_at, "");
        // `first` is now stale.
        assert!(s.erd_save(&first).is_err());
    }

    #[test]
    fn duplicate_names_are_rejected_per_connection() {
        let s = Store::open_in_memory().unwrap();
        let a = conn(&s, "a");
        let b = conn(&s, "b");
        s.erd_save(&layout(a, "Main")).unwrap();
        let err = s.erd_save(&layout(a, "Main")).unwrap_err();
        assert!(matches!(err, Error::Other(m) if m.contains("already exists")));
        // Same name on another connection is fine.
        s.erd_save(&layout(b, "Main")).unwrap();
        assert!(s.erd_save(&layout(a, "  ")).is_err());
    }

    #[test]
    fn unknown_connection_is_not_found_and_cascade_deletes() {
        let s = Store::open_in_memory().unwrap();
        assert!(matches!(
            s.erd_save(&layout(Uuid::new_v4(), "x")),
            Err(Error::NotFound(_))
        ));
        let c = conn(&s, "pg");
        let l = s.erd_save(&layout(c, "Main")).unwrap();
        s.delete_profile(c).unwrap();
        assert!(matches!(s.erd_get(l.id), Err(Error::NotFound(_))));
    }
}
