//! Saved queries and snippets (`saved_query_folders`, `saved_queries`). Package B owns this file.

use dbstudio_core::{DbKind, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{Store, not_implemented};

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

impl Store {
    /// All folders and queries.
    pub fn saved_snapshot(&self) -> Result<SavedQueriesSnapshot> {
        not_implemented("saved_snapshot")
    }

    /// Insert or update a folder (parent must exist, no cycles).
    pub fn saved_save_folder(&self, folder: &SavedFolder) -> Result<SavedFolder> {
        let _ = folder;
        not_implemented("saved_save_folder")
    }

    /// Delete a folder; its subfolders are deleted, its queries move to the top level.
    pub fn saved_delete_folder(&self, id: Uuid) -> Result<()> {
        let _ = id;
        not_implemented("saved_delete_folder")
    }

    /// Insert or update a query (timestamps maintained by the store).
    pub fn saved_save_query(&self, query: &SavedQuery) -> Result<SavedQuery> {
        let _ = query;
        not_implemented("saved_save_query")
    }

    /// Delete a query.
    pub fn saved_delete_query(&self, id: Uuid) -> Result<()> {
        let _ = id;
        not_implemented("saved_delete_query")
    }
}
