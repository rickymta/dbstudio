//! MySQL/MariaDB driver: data editing (editability, preview, apply). Owned by work package D.
//!
//! Stub created by the contract scaffold (step 0); the owning work package replaces the bodies.

use dbstudio_core::{ApplyResult, ChangeSet, Editability, Error, ObjectPath, Result};

use crate::session::MySqlSession;

/// Editability of a table; falls back to the core implementation based on `table_details`.
pub(crate) async fn editability(session: &MySqlSession, path: &ObjectPath) -> Result<Editability> {
    dbstudio_core::fallback::editability_from_details(session, path).await
}

/// The statements `apply` would run.
pub(crate) async fn preview(session: &MySqlSession, changes: &ChangeSet) -> Result<Vec<String>> {
    let _ = (session, changes);
    Err(Error::Unsupported("data_editing".into()))
}

/// Apply a change set.
pub(crate) async fn apply(session: &MySqlSession, changes: ChangeSet) -> Result<ApplyResult> {
    let _ = (session, changes);
    Err(Error::Unsupported("data_editing".into()))
}
