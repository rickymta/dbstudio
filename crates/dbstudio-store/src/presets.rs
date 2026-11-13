//! Saved export presets (`export_presets`). Package G owns this file.

use dbstudio_core::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{Store, not_implemented};

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

impl Store {
    /// Presets of a connection plus the global ones (or all when `connection_id` is `None`).
    pub fn presets_list(&self, connection_id: Option<Uuid>) -> Result<Vec<ExportPreset>> {
        let _ = connection_id;
        not_implemented("presets_list")
    }

    /// Insert or update (by id).
    pub fn presets_save(&self, preset: &ExportPreset) -> Result<ExportPreset> {
        let _ = preset;
        not_implemented("presets_save")
    }

    /// Delete a preset.
    pub fn presets_delete(&self, id: Uuid) -> Result<()> {
        let _ = id;
        not_implemented("presets_delete")
    }
}
