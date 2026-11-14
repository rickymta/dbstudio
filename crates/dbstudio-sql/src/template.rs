//! Script templates for a table: `SELECT` / `INSERT` / `UPDATE` / `DELETE` (BROWSE-05).
//!
//! Contract scaffold of the backlog contract v2: the signature is final; package S fills in the
//! per-dialect bodies (until then [`generate`] returns an empty script).

use dbstudio_core::{ObjectPath, ScriptDialect, TableDetails};
use serde::{Deserialize, Serialize};

/// Which template to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScriptTemplate {
    /// `SELECT` of the columns with a row limit.
    Select,
    /// `INSERT` with one parameter per column.
    Insert,
    /// `UPDATE` addressed by the primary / unique key.
    Update,
    /// `DELETE` addressed by the primary / unique key.
    Delete,
}

/// Pure text generation: the script for `template` on the table `path` described by `details`,
/// written in `dialect` (quoting, row limit syntax, parameter style).
pub fn generate(
    dialect: ScriptDialect,
    details: &TableDetails,
    path: &ObjectPath,
    template: ScriptTemplate,
) -> String {
    let _ = (dialect, details, path, template);
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_names_match_the_ipc_contract() {
        for (t, s) in [
            (ScriptTemplate::Select, "select"),
            (ScriptTemplate::Insert, "insert"),
            (ScriptTemplate::Update, "update"),
            (ScriptTemplate::Delete, "delete"),
        ] {
            assert_eq!(serde_json::to_value(t).unwrap(), s);
        }
    }
}
