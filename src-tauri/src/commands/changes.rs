//! Data editing commands (package D): editability, change preview and apply.

use dbstudio_core::{
    ApplyResult, ChangeSet, CollectingSink, ColumnMeta, ConnectionConfig, DbKind, Editability,
    Error, NotEditableReason, ObjectPath, PathContext, QueryRequest, ResultEditability,
    ResultNotEditableReason, RowChange, ScriptDialect, Session, StatementOutcome, TxOptions,
    TxStatus, resolve_result_editability, single_source, table_path,
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

/// Whether the rows of a result set (described by its columns) can be edited, and by which table
/// and key. `lease_id` is the editor tab's connection: its current database / schema complete
/// origins that name only a table.
pub(crate) async fn result_editability_impl(
    state: &AppState,
    session_id: Uuid,
    lease_id: Option<Uuid>,
    columns: Vec<ColumnMeta>,
) -> Result<ResultEditability> {
    let (session, config) = state.sessions.get_with_config(session_id)?;
    let n = columns.len();
    let no = |reason| Ok(ResultEditability::not_editable(n, reason));
    if matches!(config.kind, DbKind::MongoDb | DbKind::Redis) {
        return no(ResultNotEditableReason::Unsupported);
    }
    let source = match single_source(&columns) {
        Ok(s) => s,
        Err(reason) => return no(reason),
    };
    let lease_state = lease_id
        .and_then(|l| state.leases.get_with_session(l).ok())
        .filter(|(owner, _)| *owner == session_id)
        .map(|(_, lease)| lease.state());
    let ctx = PathContext {
        database: lease_state
            .as_ref()
            .and_then(|s| s.database.clone())
            .or_else(|| config.database.clone()),
        schema: lease_state.as_ref().and_then(|s| s.schema.clone()),
        user: config.username.clone(),
    };
    let Some(path) = table_path(config.kind, &source, &ctx) else {
        return no(ResultNotEditableReason::Unsupported);
    };
    let ed = match session.editability(&path).await {
        Ok(ed) => ed,
        // The table the origin names is not (or no longer) there: nothing to edit by key.
        Err(Error::NotFound(_) | Error::Unsupported(_)) => {
            return no(ResultNotEditableReason::Unsupported);
        }
        Err(e) => return Err(e),
    };
    Ok(resolve_result_editability(
        &columns,
        path,
        apply_read_only(ed, &config),
    ))
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

/// Apply `changes`. With `lease_id` (an editor tab's connection) and a transaction open on it (or
/// a manual-commit lease that has none yet), the changes run INSIDE that transaction: the tab
/// sees its own uncommitted rows and commits or rolls back together. Without one, or on an
/// auto-commit lease, they are applied by the session in a transaction of their own.
pub(crate) async fn apply_impl(
    state: &AppState,
    session_id: Uuid,
    changes: ChangeSet,
    lease_id: Option<Uuid>,
) -> Result<ApplyResult> {
    let (session, config) = state.sessions.get_with_config(session_id)?;
    guard::ensure_writable(&config, "data editing")?;
    match lease_id {
        None => session.apply_changes(changes).await,
        Some(lease_id) => apply_in_lease(state, &*session, session_id, lease_id, changes).await,
    }
}

/// Position in `changes` of each statement `preview_changes` renders: deletes, then updates, then
/// inserts, stable within each group (the order every driver applies them in).
fn application_order(changes: &[RowChange]) -> Vec<usize> {
    let group = |c: &RowChange| match c {
        RowChange::Delete { .. } => 0,
        RowChange::Update { .. } => 1,
        RowChange::Insert { .. } => 2,
    };
    let mut order: Vec<usize> = (0..changes.len()).collect();
    order.sort_by_key(|&i| group(&changes[i]));
    order
}

const SAVEPOINT: &str = "dbstudio_edit";

/// `(set, rollback)` statements of a savepoint, for the dialects that have them.
fn savepoint_sql(d: ScriptDialect) -> Option<(String, String)> {
    match d {
        ScriptDialect::MsSql => Some((
            format!("SAVE TRANSACTION {SAVEPOINT}"),
            format!("ROLLBACK TRANSACTION {SAVEPOINT}"),
        )),
        ScriptDialect::Postgres | ScriptDialect::MySql { .. } | ScriptDialect::Oracle => Some((
            format!("SAVEPOINT {SAVEPOINT}"),
            format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"),
        )),
        _ => None,
    }
}

async fn apply_in_lease(
    state: &AppState,
    session: &dyn Session,
    session_id: Uuid,
    lease_id: Uuid,
    changes: ChangeSet,
) -> Result<ApplyResult> {
    state.leases.ensure_not_debugging(lease_id)?;
    let (owner, lease) = state.leases.get_with_session(lease_id)?;
    if owner != session_id {
        return Err(Error::Invalid(
            "the connection belongs to another session".into(),
        ));
    }
    let ChangeSet::Rows(rows) = &changes else {
        return Err(Error::Unsupported("data_editing".into()));
    };
    let tx = lease.state();
    // No transaction to share: the same as editing from the table view.
    if tx.auto_commit && tx.transaction == TxStatus::Idle {
        return session.apply_changes(changes).await;
    }
    let order = application_order(&rows.changes);
    let statements = session.preview_changes(&changes).await?;
    if statements.len() != order.len() {
        return Err(Error::Invalid(
            "the driver rendered a different number of statements than changes".into(),
        ));
    }
    let dialect = lease
        .script_dialect()
        .unwrap_or_else(|| session.script_dialect());
    let savepoint = savepoint_sql(dialect);
    // A manual-commit lease opens its transaction with the first statement; open it now so the
    // savepoint has one to live in.
    if !tx.auto_commit && tx.transaction == TxStatus::Idle {
        lease.begin(TxOptions::default()).await?;
    }
    let result = run_in_lease(
        &*lease,
        &statements,
        &order,
        rows,
        dialect,
        savepoint.as_ref(),
    )
    .await;
    state.leases.notify_state(lease_id);
    result.map(|affected| ApplyResult {
        applied: affected.len() as u32,
        affected,
        atomic: savepoint.is_some(),
    })
}

async fn run_in_lease(
    lease: &dyn dbstudio_core::Lease,
    statements: &[String],
    order: &[usize],
    rows: &dbstudio_core::RowChangeSet,
    dialect: ScriptDialect,
    savepoint: Option<&(String, String)>,
) -> Result<Vec<u64>> {
    if let Some((set, _)) = savepoint {
        lease
            .execute(QueryRequest::new(set.clone()), &mut CollectingSink::new())
            .await?;
    }
    // Update / delete address one row by key. Servers that count matched rows must see exactly
    // one; MySQL-like ones report changed rows (0 when the values were already there).
    let exact = matches!(
        dialect,
        ScriptDialect::Postgres | ScriptDialect::MsSql | ScriptDialect::Oracle
    );
    let mut affected = Vec::with_capacity(statements.len());
    for (sql, &index) in statements.iter().zip(order) {
        let mut sink = CollectingSink::new();
        let outcome = lease
            .execute(QueryRequest::new(sql.clone()), &mut sink)
            .await;
        let fail = match outcome {
            Err(e) => Some(e.message()),
            Ok(_) => {
                let n = sink
                    .into_results()
                    .into_iter()
                    .find_map(|r| match r.outcome {
                        StatementOutcome::Affected { rows } => Some(rows),
                        _ => None,
                    })
                    .unwrap_or(0);
                affected.push(n);
                let keyed = !matches!(rows.changes[index], RowChange::Insert { .. });
                match n {
                    n if keyed && n > 1 => Some(format!("expected one affected row, got {n}")),
                    0 if keyed && exact => Some("expected one affected row, got 0".to_string()),
                    _ => None,
                }
            }
        };
        if let Some(message) = fail {
            if let Some((_, rollback)) = savepoint {
                // Best effort: it also revives a transaction the failure aborted.
                let _ = lease
                    .execute(
                        QueryRequest::new(rollback.clone()),
                        &mut CollectingSink::new(),
                    )
                    .await;
            }
            return Err(Error::ChangeFailed {
                index: index as u32,
                message,
                applied: 0,
            });
        }
    }
    Ok(affected)
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
pub async fn result_editability(
    state: State<'_, AppState>,
    session_id: Uuid,
    lease_id: Option<Uuid>,
    columns: Vec<ColumnMeta>,
) -> Result<ResultEditability> {
    tracing::debug!(command = "result_editability", session = %session_id, "ipc call");
    result_editability_impl(&state, session_id, lease_id, columns).await
}

#[tauri::command]
pub async fn changes_apply(
    state: State<'_, AppState>,
    session_id: Uuid,
    changes: ChangeSet,
    lease_id: Option<Uuid>,
) -> Result<ApplyResult> {
    tracing::debug!(command = "changes_apply", session = %session_id, "ipc call");
    apply_impl(&state, session_id, changes, lease_id).await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use dbstudio_core::{ColumnOrigin, DriverRegistry, Value, ValueKind};
    use dbstudio_store::{ConnectionProfile, SecretChange};

    use super::*;
    use crate::commands::{connections, sessions};
    use crate::session::testing::MockDriver;
    use crate::state::testing::test_state;

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

    fn column(name: &str, origin: Option<(&str, &str)>) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: "int8".into(),
            value_kind: ValueKind::Int,
            nullable: None,
            origin: origin.map(|(t, c)| ColumnOrigin {
                database: Some("app".into()),
                schema: Some("public".into()),
                table: t.into(),
                column: c.into(),
            }),
        }
    }

    async fn open_mock(kind: DbKind, read_only: bool) -> (AppState, Uuid) {
        let (driver, probe) = MockDriver::new(kind);
        // The mock describes a table `(id PRIMARY KEY, qty)` once it is "rich".
        probe.rich.store(true, Ordering::SeqCst);
        let mut registry = DriverRegistry::new();
        registry.register(driver);
        let (state, _) = test_state(registry);
        let mut config = ConnectionConfig::new(kind, "mock", "localhost");
        config.read_only = read_only;
        config.database = Some("app".into());
        let profile = ConnectionProfile {
            config,
            group_id: None,
            color: None,
            save_password: false,
            has_password: false,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let saved = connections::save_impl(&state, profile, SecretChange::Keep)
            .await
            .unwrap();
        let sid = sessions::open_impl(&state, saved.config.id, None)
            .await
            .unwrap()
            .session_id;
        (state, sid)
    }

    #[tokio::test]
    async fn result_editability_of_a_plain_table_result() {
        let (state, sid) = open_mock(DbKind::Postgres, false).await;
        let cols = vec![
            column("ident", Some(("t", "id"))),
            column("qty", Some(("t", "qty"))),
            column("twice", None),
        ];
        let r = result_editability_impl(&state, sid, None, cols)
            .await
            .unwrap();
        assert!(r.editable, "{r:?}");
        assert_eq!(r.key_columns, vec!["id"]);
        assert_eq!(
            r.column_map,
            vec![Some("id".into()), Some("qty".into()), None]
        );
        let path: Vec<_> = r
            .table
            .unwrap()
            .segments
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(path, ["app", "public", "t"]);
    }

    #[tokio::test]
    async fn result_editability_reasons() {
        let (state, sid) = open_mock(DbKind::Postgres, false).await;
        let reason = |cols: Vec<ColumnMeta>| {
            let state = &state;
            async move {
                let r = result_editability_impl(state, sid, None, cols)
                    .await
                    .unwrap();
                assert!(!r.editable);
                r
            }
        };
        let r = reason(vec![
            column("a", Some(("t", "id"))),
            column("b", Some(("u", "id"))),
        ])
        .await;
        assert_eq!(r.reason, Some(ResultNotEditableReason::MultipleTables));
        let r = reason(vec![column("n", None)]).await;
        assert_eq!(r.reason, Some(ResultNotEditableReason::NoOrigin));
        let r = reason(vec![column("qty", Some(("t", "qty")))]).await;
        assert_eq!(r.reason, Some(ResultNotEditableReason::KeyNotInResult));
        assert_eq!(r.missing_key_columns, vec!["id"]);
        assert!(r.table.is_some());

        // A read-only connection keeps the mapping but never edits.
        let (state, sid) = open_mock(DbKind::Postgres, true).await;
        let r = result_editability_impl(&state, sid, None, vec![column("id", Some(("t", "id")))])
            .await
            .unwrap();
        assert!(!r.editable);
        assert_eq!(r.reason, Some(ResultNotEditableReason::ReadOnlyConnection));
        assert_eq!(r.key_columns, vec!["id"]);

        // MongoDB documents are not edited here.
        let (state, sid) = open_mock(DbKind::MongoDb, false).await;
        let r = result_editability_impl(&state, sid, None, vec![column("_id", None)])
            .await
            .unwrap();
        assert_eq!(r.reason, Some(ResultNotEditableReason::Unsupported));
    }

    #[test]
    fn statements_follow_deletes_updates_inserts() {
        let upd = RowChange::Update {
            key: vec![Value::Int(1)],
            set: vec![],
        };
        let del = RowChange::Delete {
            key: vec![Value::Int(2)],
        };
        let ins = RowChange::Insert { values: vec![] };
        // As sent by the UI (already grouped) and as sent by another client.
        assert_eq!(
            application_order(&[del.clone(), upd.clone(), ins.clone()]),
            vec![0, 1, 2]
        );
        assert_eq!(
            application_order(&[ins.clone(), upd.clone(), del.clone(), upd, del]),
            vec![2, 4, 1, 3, 0]
        );
    }

    #[test]
    fn savepoints_per_dialect() {
        let (set, back) = savepoint_sql(ScriptDialect::Postgres).unwrap();
        assert_eq!(set, "SAVEPOINT dbstudio_edit");
        assert_eq!(back, "ROLLBACK TO SAVEPOINT dbstudio_edit");
        let (set, back) = savepoint_sql(ScriptDialect::MsSql).unwrap();
        assert_eq!(set, "SAVE TRANSACTION dbstudio_edit");
        assert_eq!(back, "ROLLBACK TRANSACTION dbstudio_edit");
        assert!(savepoint_sql(ScriptDialect::ClickHouse).is_none());
    }
}
