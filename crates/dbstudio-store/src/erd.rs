//! Saved ERD layouts (`erd_layouts`). Package F owns this file.

use dbstudio_core::{ObjectPath, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{Store, not_implemented};

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

impl Store {
    /// Summaries of a connection's layouts, most recently updated first.
    pub fn erd_list(&self, connection_id: Uuid) -> Result<Vec<ErdLayoutSummary>> {
        let _ = connection_id;
        not_implemented("erd_list")
    }

    /// One layout or `NotFound`.
    pub fn erd_get(&self, id: Uuid) -> Result<ErdLayout> {
        let _ = id;
        not_implemented("erd_get")
    }

    /// Insert or update (by id); a duplicate name on the connection is an error.
    pub fn erd_save(&self, layout: &ErdLayout) -> Result<ErdLayout> {
        let _ = layout;
        not_implemented("erd_save")
    }

    /// Delete a layout.
    pub fn erd_delete(&self, id: Uuid) -> Result<()> {
        let _ = id;
        not_implemented("erd_delete")
    }
}
