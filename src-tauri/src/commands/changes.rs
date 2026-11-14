//! Data editing commands (package D): editability, change preview and apply.

use dbstudio_core::{
    ApplyResult, ChangeSet, ConnectionConfig, Editability, Error, NotEditableReason, ObjectPath,
};
use tauri::State;
use uuid::Uuid;

use crate::guard;
use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

pub(crate) async fn editability_impl(
    state: &AppState,
    session_id: Uuid,
    path: ObjectPath,
) -> Result<Editability> {
    let (session, config) = state.sessions.get_with_config(session_id)?;
    let ed = session.editability(&path).await?;
    Ok(apply_read_only(ed, &config))
}

/// A read-only connection never edits: keep the key information (the grid still needs it for
/// copy formats) but report why editing is off. A more specific reason (not a table, no unique
/// key) is kept because it explains more than the connection flag.
fn apply_read_only(mut ed: Editability, config: &ConnectionConfig) -> Editability {
    if config.read_only && ed.editable {
        ed.editable = false;
        ed.reason = Some(NotEditableReason::ReadOnlyConnection);
    }
    ed
}

pub(crate) async fn preview_impl(
    state: &AppState,
    session_id: Uuid,
    changes: ChangeSet,
) -> Result<Vec<String>> {
    // Preview only renders text and never touches the server, so it is allowed on read-only
    // connections ("copy as INSERT"); `apply_impl` is the one that enforces read-only.
    let session = state.sessions.get(session_id)?;
    session.preview_changes(&changes).await
}

pub(crate) async fn apply_impl(
    state: &AppState,
    session_id: Uuid,
    changes: ChangeSet,
) -> Result<ApplyResult> {
    let (session, config) = state.sessions.get_with_config(session_id)?;
    guard::ensure_writable(&config, "data editing")?;
    session.apply_changes(changes).await
}

#[tauri::command]
pub async fn table_editability(
    state: State<'_, AppState>,
    session_id: Uuid,
    path: ObjectPath,
) -> Result<Editability> {
    tracing::debug!(command = "table_editability", session = %session_id, "ipc call");
    editability_impl(&state, session_id, path).await
}

#[tauri::command]
pub async fn changes_preview(
    state: State<'_, AppState>,
    session_id: Uuid,
    changes: ChangeSet,
) -> Result<Vec<String>> {
    tracing::debug!(command = "changes_preview", session = %session_id, "ipc call");
    preview_impl(&state, session_id, changes).await
}

#[tauri::command]
pub async fn changes_apply(
    state: State<'_, AppState>,
    session_id: Uuid,
    changes: ChangeSet,
) -> Result<ApplyResult> {
    tracing::debug!(command = "changes_apply", session = %session_id, "ipc call");
    apply_impl(&state, session_id, changes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::DbKind;

    fn ed(editable: bool, reason: Option<NotEditableReason>) -> Editability {
        Editability {
            editable,
            key_columns: vec!["id".into()],
            read_only_columns: vec![],
            reason,
            atomic: true,
            notes: vec![],
        }
    }

    #[test]
    fn read_only_connection_turns_editing_off_with_a_reason() {
        let mut cfg = ConnectionConfig::new(DbKind::Postgres, "c", "h");
        let kept = apply_read_only(ed(true, None), &cfg);
        assert!(kept.editable && kept.reason.is_none());
        cfg.read_only = true;
        let r = apply_read_only(ed(true, None), &cfg);
        assert!(!r.editable);
        assert_eq!(r.reason, Some(NotEditableReason::ReadOnlyConnection));
        assert_eq!(r.key_columns, vec!["id"]);
        let r = apply_read_only(ed(false, Some(NotEditableReason::NoUniqueKey)), &cfg);
        assert_eq!(r.reason, Some(NotEditableReason::NoUniqueKey));
    }

    #[test]
    fn apply_is_rejected_on_read_only_connections() {
        let mut cfg = ConnectionConfig::new(DbKind::Postgres, "c", "h");
        cfg.read_only = true;
        assert!(matches!(
            guard::ensure_writable(&cfg, "data editing"),
            Err(Error::ReadOnlyViolation { .. })
        ));
    }
}
