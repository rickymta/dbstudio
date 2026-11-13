//! Persisted editor tabs (`editor_tabs`).

use dbstudio_core::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{Store, db_err, now_iso, opt_uuid, parse_uuid};

/// The restorable state of one SQL editor tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorTabState {
    /// Tab id.
    pub id: Uuid,
    /// Order among the tabs.
    pub position: u32,
    /// Tab title.
    pub title: String,
    /// Connection the tab is bound to.
    pub connection_id: Option<Uuid>,
    /// Current database.
    pub database: Option<String>,
    /// Current schema.
    pub schema: Option<String>,
    /// Editor text.
    pub text: String,
    /// File the text was opened from / saved to.
    pub file_path: Option<String>,
    /// Cursor offset in characters.
    pub cursor: u32,
    /// Auto-commit mode of the tab's lease.
    pub auto_commit: bool,
}

impl Store {
    /// All saved tabs ordered by position.
    pub fn tabs_get(&self) -> Result<Vec<EditorTabState>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, position, title, connection_id, database_name, schema_name, text,
                        file_path, cursor, auto_commit
                 FROM editor_tabs ORDER BY position, updated_at, id",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, bool>(9)?,
                ))
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        rows.into_iter()
            .map(
                |(id, position, title, conn_id, database, schema, text, file_path, cursor, ac)| {
                    Ok(EditorTabState {
                        id: parse_uuid(&id)?,
                        position: position.clamp(0, i64::from(u32::MAX)) as u32,
                        title,
                        connection_id: opt_uuid(conn_id)?,
                        database,
                        schema,
                        text,
                        file_path,
                        cursor: cursor.clamp(0, i64::from(u32::MAX)) as u32,
                        auto_commit: ac,
                    })
                },
            )
            .collect()
    }

    /// Replace the saved tabs with `tabs` (atomically). A tab bound to a connection that no
    /// longer exists is stored unbound; a repeated tab id keeps its last occurrence.
    pub fn tabs_set(&self, tabs: &[EditorTabState]) -> Result<()> {
        let now = now_iso();
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute("DELETE FROM editor_tabs", []).map_err(db_err)?;
        for tab in tabs {
            tx.execute(
                "INSERT OR REPLACE INTO editor_tabs (id, position, title, connection_id,
                     database_name, schema_name, text, file_path, cursor, auto_commit, updated_at)
                 VALUES (?1, ?2, ?3, (SELECT id FROM connections WHERE id = ?4), ?5, ?6, ?7, ?8,
                     ?9, ?10, ?11)",
                params![
                    tab.id.to_string(),
                    i64::from(tab.position),
                    tab.title,
                    tab.connection_id.map(|c| c.to_string()),
                    tab.database,
                    tab.schema,
                    tab.text,
                    tab.file_path,
                    i64::from(tab.cursor),
                    tab.auto_commit,
                    now,
                ],
            )
            .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ConnectionProfile;
    use dbstudio_core::{ConnectionConfig, DbKind};

    fn tab(position: u32, text: &str, connection_id: Option<Uuid>) -> EditorTabState {
        EditorTabState {
            id: Uuid::new_v4(),
            position,
            title: format!("Query {position}"),
            connection_id,
            database: Some("app".into()),
            schema: Some("public".into()),
            text: text.into(),
            file_path: Some("/tmp/q.sql".into()),
            cursor: 7,
            auto_commit: false,
        }
    }

    fn connection(s: &Store) -> Uuid {
        s.save_profile(&ConnectionProfile {
            config: ConnectionConfig::new(DbKind::Postgres, "c", "localhost"),
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

    #[test]
    fn set_replaces_and_get_orders_by_position() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.tabs_get().unwrap().is_empty());
        let conn = connection(&s);
        let (a, b) = (tab(1, "select 1", Some(conn)), tab(0, "select 'é😀'", None));
        s.tabs_set(&[a.clone(), b.clone()]).unwrap();
        assert_eq!(s.tabs_get().unwrap(), vec![b.clone(), a.clone()]);
        s.tabs_set(std::slice::from_ref(&a)).unwrap();
        assert_eq!(s.tabs_get().unwrap(), vec![a]);
        s.tabs_set(&[]).unwrap();
        assert!(s.tabs_get().unwrap().is_empty());
    }

    #[test]
    fn unknown_connection_is_stored_unbound_and_deleting_it_unbinds() {
        let s = Store::open_in_memory().unwrap();
        let conn = connection(&s);
        let ghost = tab(0, "x", Some(Uuid::new_v4()));
        let bound = tab(1, "y", Some(conn));
        s.tabs_set(&[ghost.clone(), bound.clone()]).unwrap();
        let got = s.tabs_get().unwrap();
        assert_eq!(got[0].connection_id, None);
        assert_eq!(got[1].connection_id, Some(conn));
        s.delete_profile(conn).unwrap();
        assert_eq!(s.tabs_get().unwrap()[1].connection_id, None);
    }

    #[test]
    fn tabs_survive_reopening_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let t = tab(0, "select 1", None);
        Store::open(&path)
            .unwrap()
            .tabs_set(std::slice::from_ref(&t))
            .unwrap();
        assert_eq!(Store::open(&path).unwrap().tabs_get().unwrap(), vec![t]);
    }
}
