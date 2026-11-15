//! Data editing commands (package D): editability, change preview and apply.

use dbstudio_core::{
    ApplyResult, ChangeSet, CollectingSink, ColumnMeta, ConnectionConfig, DbKind, EditValue,
    Editability, Error, NotEditableReason, ObjectPath, PathContext, QueryRequest,
    ResultEditability, ResultNotEditableReason, RowChange, RowChangeSet, ScriptDialect, Session,
    StatementOutcome, TxOptions, TxStatus, Value, resolve_result_editability, single_source,
    table_path,
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

/// `SELECT` that gives the schema SQL Server resolves the unqualified `table` of `db` to.
fn mssql_schema_probe(db: &str, table: &str) -> String {
    let quote = |s: &str| s.replace(']', "]]");
    let name = format!("[{}]..[{}]", quote(db), quote(table)).replace('\'', "''");
    let db_lit = db.replace('\'', "''");
    format!("SELECT OBJECT_SCHEMA_NAME(OBJECT_ID(N'{name}'), DB_ID(N'{db_lit}'))")
}

async fn mssql_default_schema(session: &dyn Session, db: &str, table: &str) -> Option<String> {
    let mut sink = CollectingSink::new();
    session
        .execute(QueryRequest::new(mssql_schema_probe(db, table)), &mut sink)
        .await
        .ok()?;
    sink.into_results()
        .into_iter()
        .find_map(|r| match r.outcome {
            StatementOutcome::Rows(rs) => match rs.rows.first()?.first()? {
                dbstudio_core::Value::Text(s) if !s.is_empty() => Some(s.clone()),
                _ => None,
            },
            _ => None,
        })
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
    let _ = lease_id; // the tab's connection says nothing about where the statement ran
    let n = columns.len();
    let no = |reason| Ok(ResultEditability::not_editable(n, reason));
    if matches!(config.kind, DbKind::MongoDb | DbKind::Redis) {
        return no(ResultNotEditableReason::Unsupported);
    }
    let mut source = match single_source(&columns) {
        Ok(s) => s,
        Err(reason) => return no(reason),
    };
    // Where the statement ran is pinned in the origins when it ran (`origin.rs`, the drivers);
    // the lease's CURRENT database says nothing about it (the tab may have moved since).
    let ctx = PathContext {
        database: config.database.clone(),
        schema: None,
        user: config.username.clone(),
    };
    // SQL Server resolves an unqualified name through the user's default schema, which only
    // the server knows.
    if config.kind == DbKind::SqlServer && source.schema.is_none() {
        let Some(db) = source.database.clone().or_else(|| config.database.clone()) else {
            return no(ResultNotEditableReason::Unsupported);
        };
        match mssql_default_schema(&*session, &db, &source.table).await {
            Some(schema) => {
                source.database = Some(db);
                source.schema = Some(schema);
            }
            None => return no(ResultNotEditableReason::Unsupported),
        }
    }
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
    // Origins found by static analysis fold names like the dialect; the server's are exact.
    let fold_case = matches!(
        config.kind,
        DbKind::SqlServer | DbKind::Oracle | DbKind::Cassandra | DbKind::ScyllaDb
    );
    Ok(resolve_result_editability(
        &columns,
        path,
        apply_read_only(ed, &config),
        fold_case,
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

/// Apply `changes`. With `lease_id` (an editor tab's connection) the changes run on that very
/// connection (its `SET ROLE`, session settings and open transaction apply): inside the open
/// transaction (a manual-commit tab starts one), or in a short one of their own in auto-commit
/// mode. Without it the session applies them in a transaction of their own.
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
        Some(lease_id) => {
            apply_in_lease(state, &*session, config.kind, session_id, lease_id, changes).await
        }
    }
}

/// Position in `changes` of each statement `preview_changes` renders. PostgreSQL / MySQL group
/// deletes, then updates, then inserts (stable); SQL Server and Oracle keep the order given.
fn application_order(changes: &[RowChange], d: ScriptDialect) -> Vec<usize> {
    let mut order: Vec<usize> = (0..changes.len()).collect();
    if matches!(d, ScriptDialect::MsSql | ScriptDialect::Oracle) {
        return order;
    }
    let group = |c: &RowChange| match c {
        RowChange::Delete { .. } => 0,
        RowChange::Update { .. } => 1,
        RowChange::Insert { .. } => 2,
    };
    order.sort_by_key(|&i| group(&changes[i]));
    order
}

const SAVEPOINT: &str = "dbstudio_edit";

/// Statements of a savepoint: set, roll back to, release (where the server has one).
struct Savepoint {
    set: String,
    rollback: String,
    release: Option<String>,
}

fn savepoint_sql(d: ScriptDialect) -> Option<Savepoint> {
    match d {
        ScriptDialect::MsSql => Some(Savepoint {
            set: format!("SAVE TRANSACTION {SAVEPOINT}"),
            rollback: format!("ROLLBACK TRANSACTION {SAVEPOINT}"),
            release: None,
        }),
        ScriptDialect::Oracle => Some(Savepoint {
            set: format!("SAVEPOINT {SAVEPOINT}"),
            rollback: format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"),
            release: None,
        }),
        ScriptDialect::Postgres | ScriptDialect::MySql { .. } => Some(Savepoint {
            set: format!("SAVEPOINT {SAVEPOINT}"),
            rollback: format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"),
            release: Some(format!("RELEASE SAVEPOINT {SAVEPOINT}")),
        }),
        _ => None,
    }
}

/// Kinds whose statements name the table without its database: they write to the CONNECTION's
/// database, so the connection must be where the path says.
fn writes_to_connection_database(kind: DbKind) -> bool {
    matches!(
        kind,
        DbKind::Postgres | DbKind::CockroachDb | DbKind::YugabyteDb | DbKind::SqlServer
    )
}

/// What the table editor's drivers check before they apply (the preview of some drivers does
/// not): the table is editable, the key is the table's key, no read-only column is written.
/// Oracle's lease path writes literals, which cap text at 4000 characters and RAW at 2000 bytes.
fn validate_rows(ed: &Editability, rs: &RowChangeSet, d: ScriptDialect) -> Result<()> {
    if !ed.editable {
        return Err(Error::Unsupported(format!(
            "table is not editable ({:?})",
            ed.reason
        )));
    }
    let keyed = rs
        .changes
        .iter()
        .any(|c| !matches!(c, RowChange::Insert { .. }));
    let same = |a: &[String], b: &[String]| {
        a.len() == b.len()
            && a.iter()
                .all(|x| b.iter().any(|y| y.eq_ignore_ascii_case(x)))
    };
    if keyed && !same(&rs.key_columns, &ed.key_columns) {
        return Err(Error::Invalid(format!(
            "key columns {:?} do not match the table's key {:?}",
            rs.key_columns, ed.key_columns
        )));
    }
    for (i, change) in rs.changes.iter().enumerate() {
        let (cells, insert) = match change {
            RowChange::Update { set, .. } => (set.as_slice(), false),
            RowChange::Insert { values } => (values.as_slice(), true),
            RowChange::Delete { .. } => continue,
        };
        for c in cells {
            let default = insert && matches!(c.value, EditValue::Default);
            if !default
                && ed
                    .read_only_columns
                    .iter()
                    .any(|r| r.eq_ignore_ascii_case(&c.column))
            {
                return Err(Error::ChangeFailed {
                    index: i as u32,
                    message: format!("column {} is read-only", c.column),
                    applied: 0,
                });
            }
            if d == ScriptDialect::Oracle {
                let too_long = match &c.value {
                    EditValue::Text(t) => t.chars().count() > 4000,
                    EditValue::Value(Value::Text(t)) => t.chars().count() > 4000,
                    EditValue::Value(Value::Bytes(b)) => b.len() > 2000,
                    _ => false,
                };
                if too_long {
                    return Err(Error::ChangeFailed {
                        index: i as u32,
                        message: format!(
                            "column {}: the value is too large to save from a query result \
                             (Oracle: text over 4000 characters, binary over 2000 bytes); \
                             edit it from the table view",
                            c.column
                        ),
                        applied: 0,
                    });
                }
            }
        }
    }
    Ok(())
}

fn path_database(path: &ObjectPath) -> Option<&str> {
    path.segments
        .iter()
        .find(|s| s.kind == dbstudio_core::ObjectKind::Database)
        .map(|s| s.name.as_str())
}

async fn apply_in_lease(
    state: &AppState,
    session: &dyn Session,
    kind: DbKind,
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
    let session_dialect = session.script_dialect();
    let dialect = lease.script_dialect().unwrap_or(session_dialect);
    if dialect != session_dialect {
        // The statements are rendered with the session's quoting rules (MySQL `sql_mode`).
        return Err(Error::Invalid(
            "the tab's connection quotes text differently from the session (sql_mode); \
             save from the table view instead"
                .into(),
        ));
    }
    let tx = lease.state();
    let in_tx = tx.transaction != TxStatus::Idle;
    // The statements name the table without its database: with the tab pointing elsewhere
    // (the database dropdown changed after the query ran) they would write into the wrong
    // database. Without a transaction the session, which connects to the path's database,
    // applies them; inside one there is no safe way, so refuse.
    if writes_to_connection_database(kind) {
        if let (Some(want), Some(have)) = (path_database(&rows.table), tx.database.as_deref()) {
            if !want.eq_ignore_ascii_case(have) {
                if in_tx {
                    return Err(Error::Invalid(format!(
                        "the result is from database {want}, but the tab is now in {have} with a \
                         transaction open: switch the tab back to {want}, or commit first"
                    )));
                }
                return session.apply_changes(changes).await;
            }
        }
    }
    let ed = session.editability(&rows.table).await?;
    validate_rows(&ed, rows, dialect)?;
    let order = application_order(&rows.changes, dialect);
    let statements = session.preview_changes(&changes).await?;
    if statements.len() != order.len() {
        return Err(Error::Invalid(
            "the driver rendered a different number of statements than changes".into(),
        ));
    }
    // Manual-commit tab: the transaction stays open for the user's Commit. Auto-commit tab:
    // one statement is atomic by itself, several get a short transaction of their own.
    let plan = if in_tx || !tx.auto_commit {
        if !in_tx {
            lease.begin(TxOptions::default()).await?;
        }
        Plan::Savepoint(savepoint_sql(dialect))
    } else if statements.len() > 1 {
        lease.begin(TxOptions::default()).await?;
        Plan::OwnTx
    } else {
        Plan::Single
    };
    let result = run_in_lease(&*lease, &statements, &order, rows, &plan).await;
    state.leases.notify_state(lease_id);
    result.map(|affected| ApplyResult {
        applied: affected.len() as u32,
        affected,
        atomic: !matches!(plan, Plan::Savepoint(None)),
    })
}

enum Plan {
    /// Inside the user's transaction, undone to a savepoint on failure (when there is one).
    Savepoint(Option<Savepoint>),
    /// A short transaction opened (and committed or rolled back) just for these changes.
    OwnTx,
    /// One statement in auto-commit mode.
    Single,
}

/// Rows a keyed statement touched, as the server reports them. MySQL reports CHANGED rows, which
/// is 0 for a no-op update, so its "Rows matched" message counts; otherwise the LAST count
/// (SQL Server reports trigger counts first).
fn touched_rows(results: &[dbstudio_core::StatementResult]) -> u64 {
    let matched = results.iter().flat_map(|r| &r.messages).find_map(|m| {
        m.strip_prefix("Rows matched: ")?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    });
    matched
        .or_else(|| {
            results.iter().rev().find_map(|r| match r.outcome {
                StatementOutcome::Affected { rows } => Some(rows),
                _ => None,
            })
        })
        .unwrap_or(0)
}

async fn run_quiet(lease: &dyn dbstudio_core::Lease, sql: &str) -> Result<()> {
    lease
        .execute(
            QueryRequest::new(sql.to_string()),
            &mut CollectingSink::new(),
        )
        .await
        .map(|_| ())
}

async fn run_in_lease(
    lease: &dyn dbstudio_core::Lease,
    statements: &[String],
    order: &[usize],
    rows: &RowChangeSet,
    plan: &Plan,
) -> Result<Vec<u64>> {
    let savepoint = match plan {
        Plan::Savepoint(Some(sp)) => Some(sp),
        _ => None,
    };
    if let Some(sp) = savepoint {
        run_quiet(lease, &sp.set).await?;
    }
    let mut affected = Vec::with_capacity(statements.len());
    for (sql, &index) in statements.iter().zip(order) {
        let mut sink = CollectingSink::new();
        let outcome = lease
            .execute(QueryRequest::new(sql.clone()), &mut sink)
            .await;
        let keyed = !matches!(rows.changes[index], RowChange::Insert { .. });
        let fail = match outcome {
            Err(e) => Some(e.message()),
            Ok(_) => {
                let n = touched_rows(&sink.into_results());
                affected.push(n);
                match n {
                    n if keyed && n > 1 => Some(format!("expected one affected row, got {n}")),
                    0 if keyed => Some("no row matched the key (0 rows affected); it may have been changed or deleted".to_string()),
                    _ => None,
                }
            }
        };
        if let Some(mut message) = fail {
            let undone = match (plan, savepoint) {
                (Plan::OwnTx, _) => lease.rollback().await,
                (_, Some(sp)) => run_quiet(lease, &sp.rollback).await,
                _ => Ok(()),
            };
            if let Err(e) = undone {
                // The failure also broke the way back: the transaction is gone or unusable.
                message = format!(
                    "{message}; undoing the earlier changes failed too ({}): the tab's \
                     transaction may have been cancelled, check it before going on",
                    e.message()
                );
            }
            return Err(Error::ChangeFailed {
                index: index as u32,
                message,
                applied: 0,
            });
        }
    }
    if matches!(plan, Plan::OwnTx) {
        lease.commit().await?;
    } else if let Some(release) = savepoint.and_then(|sp| sp.release.as_ref()) {
        let _ = run_quiet(lease, release).await;
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
                alias: None,
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
    fn statements_follow_the_drivers_order() {
        let upd = RowChange::Update {
            key: vec![Value::Int(1)],
            set: vec![],
        };
        let del = RowChange::Delete {
            key: vec![Value::Int(2)],
        };
        let ins = RowChange::Insert { values: vec![] };
        let pg = ScriptDialect::Postgres;
        assert_eq!(
            application_order(&[del.clone(), upd.clone(), ins.clone()], pg),
            vec![0, 1, 2]
        );
        assert_eq!(
            application_order(&[ins.clone(), upd.clone(), del.clone(), upd, del], pg),
            vec![2, 4, 1, 3, 0]
        );
        // SQL Server and Oracle render in the order given.
        assert_eq!(
            application_order(
                &[ins, RowChange::Delete { key: vec![] }],
                ScriptDialect::MsSql
            ),
            vec![0, 1]
        );
    }

    #[test]
    fn savepoints_per_dialect() {
        let sp = savepoint_sql(ScriptDialect::Postgres).unwrap();
        assert_eq!(sp.set, "SAVEPOINT dbstudio_edit");
        assert_eq!(sp.rollback, "ROLLBACK TO SAVEPOINT dbstudio_edit");
        assert_eq!(
            sp.release.as_deref(),
            Some("RELEASE SAVEPOINT dbstudio_edit")
        );
        let sp = savepoint_sql(ScriptDialect::MsSql).unwrap();
        assert_eq!(sp.set, "SAVE TRANSACTION dbstudio_edit");
        assert_eq!(sp.rollback, "ROLLBACK TRANSACTION dbstudio_edit");
        assert!(sp.release.is_none());
        assert!(savepoint_sql(ScriptDialect::ClickHouse).is_none());
    }

    fn result(outcome: StatementOutcome, messages: &[&str]) -> dbstudio_core::StatementResult {
        dbstudio_core::StatementResult {
            outcome,
            elapsed_ms: 0,
            messages: messages.iter().map(|m| (*m).to_string()).collect(),
        }
    }

    #[test]
    fn touched_rows_reads_the_matched_count_or_the_last_affected_count() {
        // MySQL: 0 changed rows, 1 matched (an update to the same value).
        let r = [result(
            StatementOutcome::Affected { rows: 0 },
            &["Rows matched: 1  Changed: 0  Warnings: 0"],
        )];
        assert_eq!(touched_rows(&r), 1);
        // MySQL: the key matched nothing.
        let r = [result(
            StatementOutcome::Affected { rows: 0 },
            &["Rows matched: 0  Changed: 0  Warnings: 0"],
        )];
        assert_eq!(touched_rows(&r), 0);
        // SQL Server: a trigger reports first, the statement last.
        let r = [
            result(StatementOutcome::Affected { rows: 5 }, &[]),
            result(StatementOutcome::Affected { rows: 1 }, &[]),
        ];
        assert_eq!(touched_rows(&r), 1);
    }

    fn rs(table_key: &[&str], changes: Vec<RowChange>) -> RowChangeSet {
        RowChangeSet {
            table: ObjectPath::root(),
            key_columns: table_key.iter().map(|s| (*s).into()).collect(),
            changes,
        }
    }

    fn cell(c: &str, v: EditValue) -> dbstudio_core::CellEdit {
        dbstudio_core::CellEdit {
            column: c.into(),
            value: v,
        }
    }

    #[test]
    fn rows_are_validated_like_the_table_editor_does() {
        let ed = Editability {
            editable: true,
            key_columns: vec!["id".into()],
            read_only_columns: vec!["gen".into()],
            reason: None,
            atomic: true,
            notes: vec![],
        };
        let pg = ScriptDialect::Postgres;
        let upd = |c: &str| RowChange::Update {
            key: vec![Value::Int(1)],
            set: vec![cell(c, EditValue::Text("x".into()))],
        };
        assert!(validate_rows(&ed, &rs(&["id"], vec![upd("a")]), pg).is_ok());
        // A key that is not the table's key.
        assert!(matches!(
            validate_rows(&ed, &rs(&["a"], vec![upd("b")]), pg),
            Err(Error::Invalid(_))
        ));
        // A generated column; DEFAULT for it on insert is fine.
        assert!(matches!(
            validate_rows(&ed, &rs(&["id"], vec![upd("gen")]), pg),
            Err(Error::ChangeFailed { index: 0, .. })
        ));
        let ins = RowChange::Insert {
            values: vec![cell("gen", EditValue::Default)],
        };
        assert!(validate_rows(&ed, &rs(&["id"], vec![ins]), pg).is_ok());
        // Not editable at all.
        let mut no = ed.clone();
        no.editable = false;
        assert!(validate_rows(&no, &rs(&["id"], vec![upd("a")]), pg).is_err());
        // Oracle literals are capped.
        let big = RowChange::Update {
            key: vec![Value::Int(1)],
            set: vec![cell("a", EditValue::Text("x".repeat(4001)))],
        };
        assert!(validate_rows(&ed, &rs(&["id"], vec![big.clone()]), pg).is_ok());
        assert!(matches!(
            validate_rows(&ed, &rs(&["id"], vec![big]), ScriptDialect::Oracle),
            Err(Error::ChangeFailed { .. })
        ));
    }

    #[test]
    fn only_connections_that_write_to_their_own_database_need_the_check() {
        assert!(writes_to_connection_database(DbKind::Postgres));
        assert!(writes_to_connection_database(DbKind::SqlServer));
        assert!(!writes_to_connection_database(DbKind::MySql));
        assert!(!writes_to_connection_database(DbKind::Oracle));
    }

    #[test]
    fn the_sql_server_schema_probe_quotes_names() {
        assert_eq!(
            mssql_schema_probe("my]db", "o'rders"),
            "SELECT OBJECT_SCHEMA_NAME(OBJECT_ID(N'[my]]db]..[o''rders]'), DB_ID(N'my]db'))"
        );
    }
}
