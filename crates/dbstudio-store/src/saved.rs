//! Saved queries and snippets (`saved_query_folders`, `saved_queries`).

use dbstudio_core::{DbKind, Error, Result};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{Store, db_err, from_text, now_iso, opt_uuid, parse_uuid, to_text};

/// A folder of saved queries (folders nest).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedFolder {
    /// Folder id.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Parent folder, `None` at the top level.
    pub parent_id: Option<Uuid>,
    /// Position among siblings.
    pub sort_order: i64,
}

/// A saved query or snippet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedQuery {
    /// Query id.
    pub id: Uuid,
    /// Containing folder.
    pub folder_id: Option<Uuid>,
    /// Display name.
    pub name: String,
    /// Statement text (snippets use `${1:name}` placeholders).
    pub text: String,
    /// True for a snippet.
    pub is_snippet: bool,
    /// Restrict to one engine; `None` = any.
    pub db_kind: Option<DbKind>,
    /// Restrict to one connection.
    pub connection_id: Option<Uuid>,
    /// Free-form description.
    pub description: Option<String>,
    /// Position among siblings.
    pub sort_order: i64,
    /// ISO-8601 creation time.
    pub created_at: String,
    /// ISO-8601 last update time.
    pub updated_at: String,
}

/// Every folder and saved query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedQueriesSnapshot {
    /// All folders.
    pub folders: Vec<SavedFolder>,
    /// All queries.
    pub queries: Vec<SavedQuery>,
}

/// Longest folder / query name, in characters.
const NAME_MAX_CHARS: usize = 200;

fn fk_not_found(what: &str) -> impl Fn(rusqlite::Error) -> Error + '_ {
    move |e| match e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Error::NotFound(what.to_string())
        }
        other => db_err(other),
    }
}

fn check_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Invalid("the name must not be empty".into()));
    }
    if name.chars().count() > NAME_MAX_CHARS {
        return Err(Error::Invalid(format!(
            "the name must be at most {NAME_MAX_CHARS} characters"
        )));
    }
    Ok(name.to_string())
}

impl Store {
    /// All folders and queries.
    pub fn saved_snapshot(&self) -> Result<SavedQueriesSnapshot> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, parent_id, sort_order FROM saved_query_folders
                 ORDER BY sort_order, name COLLATE NOCASE, id",
            )
            .map_err(db_err)?;
        let folders = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?
            .into_iter()
            .map(|(id, name, parent, sort_order)| {
                Ok(SavedFolder {
                    id: parse_uuid(&id)?,
                    name,
                    parent_id: opt_uuid(parent)?,
                    sort_order,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let mut stmt = conn
            .prepare(
                "SELECT id, folder_id, name, text, is_snippet, db_kind, connection_id,
                        description, sort_order, created_at, updated_at
                 FROM saved_queries ORDER BY sort_order, name COLLATE NOCASE, id",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, bool>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, String>(10)?,
                ))
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        let mut queries = Vec::with_capacity(rows.len());
        for (id, folder, name, text, is_snippet, kind, conn_id, description, sort_order, c, u) in
            rows
        {
            queries.push(SavedQuery {
                id: parse_uuid(&id)?,
                folder_id: opt_uuid(folder)?,
                name,
                text,
                is_snippet,
                db_kind: kind.as_deref().map(from_text::<DbKind>).transpose()?,
                connection_id: opt_uuid(conn_id)?,
                description,
                sort_order,
                created_at: c,
                updated_at: u,
            });
        }
        Ok(SavedQueriesSnapshot { folders, queries })
    }

    /// Insert or update a folder (parent must exist, no cycles).
    pub fn saved_save_folder(&self, folder: &SavedFolder) -> Result<SavedFolder> {
        let name = check_name(&folder.name)?;
        let conn = self.lock();
        if let Some(parent) = folder.parent_id {
            // Walk up from the parent: meeting the folder itself would close a cycle.
            let mut cursor = Some(parent);
            let mut hops = 0;
            while let Some(current) = cursor {
                if current == folder.id {
                    return Err(Error::Invalid(
                        "a folder cannot be moved into itself or its subfolders".into(),
                    ));
                }
                let next: Option<Option<String>> = conn
                    .query_row(
                        "SELECT parent_id FROM saved_query_folders WHERE id = ?1",
                        params![current.to_string()],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(db_err)?;
                match next {
                    None => {
                        return Err(Error::NotFound(format!("saved folder {current}")));
                    }
                    Some(p) => cursor = opt_uuid(p)?,
                }
                hops += 1;
                if hops > 10_000 {
                    return Err(Error::Storage("invalid data: folder tree too deep".into()));
                }
            }
        }
        // Folder names are unique among the children of one parent (the top level included).
        let clash: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM saved_query_folders
                     WHERE parent_id IS ?1 AND id <> ?2 AND name = ?3 COLLATE NOCASE)",
                params![
                    folder.parent_id.map(|p| p.to_string()),
                    folder.id.to_string(),
                    name
                ],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if clash {
            return Err(Error::Invalid(format!(
                "a folder named `{name}` already exists in this location"
            )));
        }
        conn.execute(
            "INSERT INTO saved_query_folders (id, name, parent_id, sort_order)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name,
                 parent_id = excluded.parent_id, sort_order = excluded.sort_order",
            params![
                folder.id.to_string(),
                name,
                folder.parent_id.map(|p| p.to_string()),
                folder.sort_order
            ],
        )
        .map_err(fk_not_found("saved folder parent"))?;
        Ok(SavedFolder {
            name,
            ..folder.clone()
        })
    }

    /// Delete a folder; its subfolders are deleted, its queries move to the top level.
    pub fn saved_delete_folder(&self, id: Uuid) -> Result<()> {
        let n = self
            .lock()
            .execute(
                "DELETE FROM saved_query_folders WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("saved folder {id}")));
        }
        Ok(())
    }

    /// Insert or update a query (timestamps maintained by the store).
    pub fn saved_save_query(&self, query: &SavedQuery) -> Result<SavedQuery> {
        let name = check_name(&query.name)?;
        let db_kind = query.db_kind.map(|k| to_text(&k)).transpose()?;
        let now = now_iso();
        let conn = self.lock();
        let created: Option<String> = conn
            .query_row(
                "SELECT created_at FROM saved_queries WHERE id = ?1",
                params![query.id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        let created_at = created.unwrap_or_else(|| now.clone());
        conn.execute(
            "INSERT INTO saved_queries (id, folder_id, name, text, is_snippet, db_kind,
                 connection_id, description, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET folder_id = excluded.folder_id, name = excluded.name,
                 text = excluded.text, is_snippet = excluded.is_snippet,
                 db_kind = excluded.db_kind, connection_id = excluded.connection_id,
                 description = excluded.description, sort_order = excluded.sort_order,
                 updated_at = excluded.updated_at",
            params![
                query.id.to_string(),
                query.folder_id.map(|f| f.to_string()),
                name,
                query.text,
                query.is_snippet,
                db_kind,
                query.connection_id.map(|c| c.to_string()),
                query.description,
                query.sort_order,
                created_at,
                now,
            ],
        )
        .map_err(fk_not_found("saved query folder or connection"))?;
        Ok(SavedQuery {
            name,
            created_at,
            updated_at: now,
            ..query.clone()
        })
    }

    /// Delete a query.
    pub fn saved_delete_query(&self, id: Uuid) -> Result<()> {
        let n = self
            .lock()
            .execute(
                "DELETE FROM saved_queries WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("saved query {id}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ConnectionProfile;
    use dbstudio_core::ConnectionConfig;

    fn folder(name: &str, parent: Option<Uuid>) -> SavedFolder {
        SavedFolder {
            id: Uuid::new_v4(),
            name: name.into(),
            parent_id: parent,
            sort_order: 0,
        }
    }

    fn query(name: &str, folder_id: Option<Uuid>) -> SavedQuery {
        SavedQuery {
            id: Uuid::new_v4(),
            folder_id,
            name: name.into(),
            text: "select 1".into(),
            is_snippet: false,
            db_kind: Some(DbKind::Postgres),
            connection_id: None,
            description: None,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn query_roundtrip_keeps_created_at_and_bumps_updated_at() {
        let s = Store::open_in_memory().unwrap();
        let q = s.saved_save_query(&query("  report  ", None)).unwrap();
        assert_eq!(q.name, "report");
        assert!(!q.created_at.is_empty());
        let mut edited = q.clone();
        edited.text = "select 2".into();
        edited.created_at = "bogus".into();
        edited.is_snippet = true;
        edited.db_kind = None;
        let q2 = s.saved_save_query(&edited).unwrap();
        assert_eq!(q2.created_at, q.created_at);
        let snap = s.saved_snapshot().unwrap();
        assert_eq!(snap.queries, vec![q2]);
        assert_eq!(snap.queries[0].text, "select 2");
        assert!(snap.queries[0].is_snippet);
        assert_eq!(snap.queries[0].db_kind, None);
        s.saved_delete_query(q.id).unwrap();
        assert!(matches!(
            s.saved_delete_query(q.id),
            Err(Error::NotFound(_))
        ));
        assert!(s.saved_snapshot().unwrap().queries.is_empty());
    }

    #[test]
    fn invalid_names_and_unknown_references_are_rejected() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.saved_save_query(&query("   ", None)).is_err());
        assert!(s.saved_save_folder(&folder("", None)).is_err());
        let long = "x".repeat(NAME_MAX_CHARS + 1);
        assert!(s.saved_save_query(&query(&long, None)).is_err());
        assert!(matches!(
            s.saved_save_query(&query("q", Some(Uuid::new_v4()))),
            Err(Error::NotFound(_))
        ));
        let mut q = query("q", None);
        q.connection_id = Some(Uuid::new_v4());
        assert!(matches!(s.saved_save_query(&q), Err(Error::NotFound(_))));
        assert!(matches!(
            s.saved_save_folder(&folder("f", Some(Uuid::new_v4()))),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn folders_nest_without_cycles_and_deleting_moves_queries_up() {
        let s = Store::open_in_memory().unwrap();
        let a = s.saved_save_folder(&folder("a", None)).unwrap();
        let b = s.saved_save_folder(&folder("b", Some(a.id))).unwrap();
        let c = s.saved_save_folder(&folder("c", Some(b.id))).unwrap();
        // a -> c would close a cycle; so would a -> a.
        let mut moved = a.clone();
        moved.parent_id = Some(c.id);
        assert!(s.saved_save_folder(&moved).is_err());
        moved.parent_id = Some(a.id);
        assert!(s.saved_save_folder(&moved).is_err());

        let in_a = s.saved_save_query(&query("in a", Some(a.id))).unwrap();
        let in_c = s.saved_save_query(&query("in c", Some(c.id))).unwrap();
        let in_top = s.saved_save_query(&query("top", None)).unwrap();
        s.saved_delete_folder(a.id).unwrap();
        let snap = s.saved_snapshot().unwrap();
        assert!(snap.folders.is_empty());
        assert_eq!(snap.queries.len(), 3);
        assert!(snap.queries.iter().all(|q| q.folder_id.is_none()));
        let ids: Vec<_> = snap.queries.iter().map(|q| q.id).collect();
        for q in [in_a, in_c, in_top] {
            assert!(ids.contains(&q.id));
        }
        assert!(matches!(
            s.saved_delete_folder(a.id),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn deleting_the_connection_detaches_its_saved_queries() {
        let s = Store::open_in_memory().unwrap();
        let conn = s
            .save_profile(&ConnectionProfile {
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
            .id;
        let mut q = query("q", None);
        q.connection_id = Some(conn);
        s.saved_save_query(&q).unwrap();
        s.delete_profile(conn).unwrap();
        assert_eq!(s.saved_snapshot().unwrap().queries[0].connection_id, None);
    }

    #[test]
    fn saved_queries_survive_reopening_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let id = {
            let s = Store::open(&path).unwrap();
            s.saved_save_query(&query("keep", None)).unwrap().id
        };
        let s = Store::open(&path).unwrap();
        assert_eq!(s.saved_snapshot().unwrap().queries[0].id, id);
    }

    #[test]
    fn renaming_a_folder_keeps_its_content_and_refuses_a_sibling_name() {
        let s = Store::open_in_memory().unwrap();
        let reports = s.saved_save_folder(&folder("Reports", None)).unwrap();
        let other = s.saved_save_folder(&folder("Other", None)).unwrap();
        let q = s.saved_save_query(&query("q", Some(reports.id))).unwrap();
        // Rename (upsert by id).
        let renamed = s
            .saved_save_folder(&SavedFolder {
                name: "  Monthly ".into(),
                ..reports.clone()
            })
            .unwrap();
        assert_eq!(renamed.name, "Monthly");
        let snap = s.saved_snapshot().unwrap();
        assert!(snap.folders.iter().any(|f| f.name == "Monthly"));
        assert_eq!(snap.queries[0].folder_id, Some(q.folder_id.unwrap()));
        // Same name as a sibling (case-insensitive): refused.
        let err = s
            .saved_save_folder(&SavedFolder {
                name: "other".into(),
                ..renamed.clone()
            })
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err:?}");
        // Saving the folder under its own name again is not a clash with itself.
        s.saved_save_folder(&renamed).unwrap();
        // The same name under another parent is fine.
        s.saved_save_folder(&folder("Monthly", Some(other.id)))
            .unwrap();
        // A new top-level folder with an existing name is refused too.
        assert!(matches!(
            s.saved_save_folder(&folder("Monthly", None)),
            Err(Error::Invalid(_))
        ));
    }
}
