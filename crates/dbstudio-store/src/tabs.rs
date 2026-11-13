//! Persisted editor tabs (`editor_tabs`). Package B owns this file.

use dbstudio_core::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{Store, not_implemented};

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
        not_implemented("tabs_get")
    }

    /// Replace the saved tabs with `tabs` (atomically).
    pub fn tabs_set(&self, tabs: &[EditorTabState]) -> Result<()> {
        let _ = tabs;
        not_implemented("tabs_set")
    }
}
