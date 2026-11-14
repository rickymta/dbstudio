//! Data editing, deliberately conservative.
//!
//! ClickHouse has no row identity and no transactions: only plain `*MergeTree` tables are
//! editable (the `Replacing`, `Collapsing`, `Aggregating`, `Summing` families accept inserts
//! only), rows are identified by the sorting key, and before every update or delete the key
//! must match exactly one row (`SELECT count()`), otherwise the change fails with
//! `ChangeFailed("key matches N rows")`. Updates and deletes are mutations run with
//! `mutations_sync = 2` (they finish before the call returns); a change set is not atomic.

use std::collections::HashMap;

use dbstudio_core::{
    ApplyResult, CellEdit, ChangeSet, EditNote, EditValue, Editability, Error, NotEditableReason,
    ObjectKind, ObjectPath, Result, RowChange, RowChangeSet, Value,
};

use crate::browse::{self, ColumnFacts, Place};
use crate::conn::{Core, Req, cell_u64, quote_ident};
use crate::sql::{literal, literal_untyped, quote_table, table_target};
use crate::types::CType;

/// How a table may be edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Full,
    InsertOnly,
    No,
}

/// Mode of an engine name (`MergeTree` family only).
pub(crate) fn engine_mode(engine: &str) -> Mode {
    let base = engine
        .strip_prefix("SharedReplicated")
        .or_else(|| engine.strip_prefix("Replicated"))
        .or_else(|| engine.strip_prefix("Shared"))
        .unwrap_or(engine);
    if base == "MergeTree" {
        Mode::Full
    } else if base.ends_with("MergeTree") {
        Mode::InsertOnly
    } else {
        Mode::No
    }
}

/// Everything known about a table for editing.
pub(crate) struct Info {
    pub ed: Editability,
    pub types: HashMap<String, CType>,
    /// Columns that cannot be written at all (MATERIALIZED / ALIAS).
    pub generated: Vec<String>,
    /// Sorting / partition key columns (cannot change by UPDATE).
    pub key_like: Vec<String>,
    pub table: String,
    pub db: String,
    pub name: String,
}

fn not_editable(reason: NotEditableReason) -> Editability {
    Editability {
        editable: false,
        key_columns: vec![],
        read_only_columns: vec![],
        reason: Some(reason),
        atomic: false,
        notes: vec![],
    }
}

pub(crate) async fn info(core: &Core, path: &ObjectPath) -> Result<Info> {
    let (db, t, kind) = match browse::resolve(path)? {
        Place::Object(d, t, k) => (d, t, k),
        _ => {
            return Ok(Info {
                ed: not_editable(NotEditableReason::NotATable),
                types: HashMap::new(),
                generated: vec![],
                key_like: vec![],
                table: String::new(),
                db: String::new(),
                name: String::new(),
            });
        }
    };
    let table = quote_table(&db, &t);
    let empty = |ed| Info {
        ed,
        types: HashMap::new(),
        generated: vec![],
        key_like: vec![],
        table: table.clone(),
        db: db.clone(),
        name: t.clone(),
    };
    if kind != ObjectKind::Table {
        return Ok(empty(not_editable(NotEditableReason::NotATable)));
    }
    let facts = browse::table_facts(core, &db, &t).await?;
    if browse::Class::of_engine(&facts.engine) != browse::Class::Table {
        return Ok(empty(not_editable(NotEditableReason::NotATable)));
    }
    let columns: Vec<ColumnFacts> = browse::column_list(core, &db, &t).await?;
    let types: HashMap<String, CType> = columns
        .iter()
        .map(|c| (c.name.clone(), CType::parse_lenient(&c.ty)))
        .collect();
    let generated: Vec<String> = columns
        .iter()
        .filter(|c| matches!(c.default_kind.as_str(), "MATERIALIZED" | "ALIAS"))
        .map(|c| c.name.clone())
        .collect();
    let key_like: Vec<String> = columns
        .iter()
        .filter(|c| c.in_sorting_key || c.in_partition_key)
        .map(|c| c.name.clone())
        .collect();
    let sorting: Vec<String> = columns
        .iter()
        .filter(|c| c.in_sorting_key)
        .map(|c| c.name.clone())
        .collect();
    let mut read_only = key_like.clone();
    for g in &generated {
        if !read_only.contains(g) {
            read_only.push(g.clone());
        }
    }
    let mode = if core.config_read_only {
        None
    } else {
        Some(engine_mode(&facts.engine))
    };
    let ed = match mode {
        None => not_editable(NotEditableReason::ReadOnlyConnection),
        Some(Mode::No) => not_editable(NotEditableReason::Unsupported),
        Some(m) => {
            let insert_only = m == Mode::InsertOnly || core.insert_only || sorting.is_empty();
            Editability {
                editable: true,
                key_columns: if insert_only { vec![] } else { sorting },
                read_only_columns: read_only,
                reason: None,
                atomic: false,
                notes: vec![EditNote::AsyncMutation],
            }
        }
    };
    Ok(Info {
        ed,
        types,
        generated,
        key_like,
        table,
        db,
        name: t,
    })
}

pub(crate) async fn editability(core: &Core, path: &ObjectPath) -> Result<Editability> {
    Ok(info(core, path).await?.ed)
}

fn bad(index: usize, message: impl Into<String>) -> Error {
    Error::ChangeFailed {
        index: index as u32,
        message: message.into(),
        applied: 0,
    }
}

fn rows_of(changes: &ChangeSet) -> Result<&RowChangeSet> {
    match changes {
        ChangeSet::Rows(rs) => Ok(rs),
        _ => Err(Error::Unsupported(
            "this connection only edits table rows".into(),
        )),
    }
}

/// A rendered change.
struct Step {
    index: usize,
    sql: String,
    /// `SELECT count()` that must return 1 first (updates, deletes).
    guard: Option<String>,
    /// Alternative statement when the lightweight delete is refused.
    fallback: Option<String>,
}

fn edit_literal(info: &Info, index: usize, column: &str, v: &EditValue) -> Result<String> {
    let lit = |v: &Value| -> Result<String> {
        match info.types.get(column) {
            Some(t) => literal(t, v),
            None => literal_untyped(v),
        }
        .map_err(|e| bad(index, e.message()))
    };
    match v {
        EditValue::Default => Ok("DEFAULT".into()),
        EditValue::Text(s) => lit(&Value::Text(s.clone())),
        EditValue::Value(v) => lit(v),
    }
}

fn key_condition(info: &Info, rs: &RowChangeSet, index: usize, key: &[Value]) -> Result<String> {
    if rs.key_columns.is_empty() || rs.key_columns.len() != key.len() {
        return Err(bad(index, "key does not match the key columns"));
    }
    let parts = rs
        .key_columns
        .iter()
        .zip(key)
        .map(|(c, v)| {
            let col = quote_ident(c);
            Ok(match v {
                Value::Null => format!("isNull({col})"),
                v => format!(
                    "{col} = {}",
                    edit_literal(info, index, c, &EditValue::Value(v.clone()))?
                ),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(parts.join(" AND "))
}

fn render_one(
    info: &Info,
    rs: &RowChangeSet,
    lightweight_delete: bool,
    index: usize,
    change: &RowChange,
) -> Result<Step> {
    let table = &info.table;
    match change {
        RowChange::Update { key, set } => {
            if set.is_empty() {
                return Err(bad(index, "update without any cell"));
            }
            let cond = key_condition(info, rs, index, key)?;
            let assigns = set
                .iter()
                .map(|c| {
                    if matches!(c.value, EditValue::Default) {
                        return Err(bad(index, "DEFAULT is only valid when inserting"));
                    }
                    Ok(format!(
                        "{} = {}",
                        quote_ident(&c.column),
                        edit_literal(info, index, &c.column, &c.value)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Step {
                index,
                sql: format!(
                    "ALTER TABLE {table} UPDATE {} WHERE {cond}",
                    assigns.join(", ")
                ),
                guard: Some(format!("SELECT count() FROM {table} WHERE {cond}")),
                fallback: None,
            })
        }
        RowChange::Delete { key } => {
            let cond = key_condition(info, rs, index, key)?;
            let alter = format!("ALTER TABLE {table} DELETE WHERE {cond}");
            Ok(Step {
                index,
                sql: if lightweight_delete {
                    format!("DELETE FROM {table} WHERE {cond}")
                } else {
                    alter.clone()
                },
                guard: Some(format!("SELECT count() FROM {table} WHERE {cond}")),
                fallback: lightweight_delete.then_some(alter),
            })
        }
        RowChange::Insert { values } => {
            if values.is_empty() {
                return Err(bad(index, "insert without any cell"));
            }
            let cols: Vec<String> = values.iter().map(|c| quote_ident(&c.column)).collect();
            let vals = values
                .iter()
                .map(|c| edit_literal(info, index, &c.column, &c.value))
                .collect::<Result<Vec<_>>>()?;
            Ok(Step {
                index,
                sql: format!(
                    "INSERT INTO {table} ({}) VALUES ({})",
                    cols.join(", "),
                    vals.join(", ")
                ),
                guard: None,
                fallback: None,
            })
        }
    }
}

/// Deletes first, then updates, then inserts (stable within each group).
fn render(info: &Info, core: &Core, rs: &RowChangeSet) -> Result<Vec<Step>> {
    let lightweight = core.version.at_least(23, 3);
    let mut steps = Vec::with_capacity(rs.changes.len());
    for want in [0u8, 1, 2] {
        for (index, change) in rs.changes.iter().enumerate() {
            let group = match change {
                RowChange::Delete { .. } => 0,
                RowChange::Update { .. } => 1,
                RowChange::Insert { .. } => 2,
            };
            if group == want {
                steps.push(render_one(info, rs, lightweight, index, change)?);
            }
        }
    }
    Ok(steps)
}

fn validate(info: &Info, rs: &RowChangeSet) -> Result<()> {
    let ed = &info.ed;
    if !ed.editable {
        return Err(Error::Unsupported(format!(
            "table is not editable ({})",
            ed.reason
                .map_or("unknown".to_string(), |r| format!("{r:?}"))
        )));
    }
    let keyed = rs
        .changes
        .iter()
        .any(|c| matches!(c, RowChange::Update { .. } | RowChange::Delete { .. }));
    if keyed {
        if ed.key_columns.is_empty() {
            return Err(Error::Unsupported(
                "this table accepts inserts only (updates and deletes are disabled)".into(),
            ));
        }
        if rs.key_columns != ed.key_columns {
            return Err(Error::Invalid(format!(
                "key columns {:?} do not match the table's sorting key {:?}",
                rs.key_columns, ed.key_columns
            )));
        }
    }
    for (i, change) in rs.changes.iter().enumerate() {
        match change {
            RowChange::Update { set, .. } => {
                for CellEdit { column, .. } in set {
                    if info.generated.contains(column) {
                        return Err(bad(i, format!("column {column} is generated (read-only)")));
                    }
                    if info.key_like.contains(column) {
                        return Err(bad(
                            i,
                            format!(
                                "column {column} is part of the sorting or partition key and cannot be updated"
                            ),
                        ));
                    }
                }
            }
            RowChange::Insert { values } => {
                for c in values {
                    if info.generated.contains(&c.column) && !matches!(c.value, EditValue::Default)
                    {
                        return Err(bad(
                            i,
                            format!("column {} is generated (read-only)", c.column),
                        ));
                    }
                }
            }
            RowChange::Delete { .. } => {}
        }
    }
    Ok(())
}

pub(crate) async fn preview(core: &Core, changes: &ChangeSet) -> Result<Vec<String>> {
    let rs = rows_of(changes)?;
    let info = info(core, &rs.table).await?;
    validate(&info, rs)?;
    Ok(render(&info, core, rs)?
        .into_iter()
        .map(|s| format!("{};", s.sql))
        .collect())
}

/// How long one update/delete may wait for its mutation to finish.
const MUTATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Ids of the unfinished mutations of the table (empty when `system.mutations` cannot be read).
async fn pending_mutations(core: &Core, info: &Info) -> Vec<String> {
    let sql = format!(
        "SELECT mutation_id FROM system.mutations WHERE database = {} AND `table` = {} AND NOT is_done",
        crate::conn::quote_literal(&info.db),
        crate::conn::quote_literal(&info.name)
    );
    core.rows(&Req::default(), &sql)
        .await
        .map(|rows| rows.iter().map(|r| crate::conn::cell_text(r, 0)).collect())
        .unwrap_or_default()
}

/// `KILL MUTATION` for the unfinished mutations that appeared since `before`.
async fn kill_new_mutations(core: &Core, info: &Info, before: &[String]) {
    for id in pending_mutations(core, info).await {
        if before.contains(&id) {
            continue;
        }
        let sql = format!(
            "KILL MUTATION WHERE database = {} AND `table` = {} AND mutation_id = {} FORMAT Null",
            crate::conn::quote_literal(&info.db),
            crate::conn::quote_literal(&info.name),
            crate::conn::quote_literal(&id)
        );
        let _ = core
            .admin_client(&Req::default())
            .query_raw(&sql)
            .execute()
            .await;
    }
}

/// Statement-level failure of the lightweight `DELETE` that justifies the `ALTER` fallback.
fn lightweight_refused(e: &Error) -> bool {
    let m = e.message();
    m.contains("Code: 48.")
        || m.contains("Code: 344.")
        || m.to_ascii_lowercase().contains("lightweight")
}

pub(crate) async fn apply(core: &Core, changes: ChangeSet) -> Result<ApplyResult> {
    let rs = rows_of(&changes)?;
    // Make sure the table name is well formed before any I/O.
    table_target(&rs.table)?;
    let info = info(core, &rs.table).await?;
    validate(&info, rs)?;
    let steps = render(&info, core, rs)?;
    let req = Req {
        settings: vec![
            ("mutations_sync".into(), "2".into()),
            ("alter_sync".into(), "2".into()),
        ],
        ..Req::default()
    };
    let mut affected = Vec::with_capacity(steps.len());
    let mut applied = 0u32;
    let fail = |index: usize, message: String, applied: u32| Error::ChangeFailed {
        index: index as u32,
        message,
        applied,
    };
    for step in &steps {
        if let Some(guard) = &step.guard {
            let n = core
                .rows(&Req::default(), guard)
                .await
                .map_err(|e| fail(step.index, e.message(), applied))?
                .first()
                .and_then(|r| cell_u64(r, 0))
                .unwrap_or(0);
            if n != 1 {
                return Err(fail(step.index, format!("key matches {n} rows"), applied));
            }
        }
        // Mutations still running that were not there before this step.
        let before = pending_mutations(core, &info).await;
        let run = async {
            let mut result = core.exec(&req, &step.sql).await;
            if let (Err(e), Some(alt)) = (&result, &step.fallback) {
                if lightweight_refused(e) {
                    result = core.exec(&req, alt).await;
                }
            }
            result
        };
        let mut result = match tokio::time::timeout(MUTATION_TIMEOUT, run).await {
            Ok(r) => r,
            Err(_) => Err(Error::Timeout),
        };
        if result.is_err() && step.guard.is_some() {
            // A failed or abandoned mutation would block every later one on the table.
            kill_new_mutations(core, &info, &before).await;
            if matches!(result, Err(Error::Timeout)) {
                result = Err(Error::Query {
                    message: format!(
                        "the mutation did not finish within {} s and was cancelled",
                        MUTATION_TIMEOUT.as_secs()
                    ),
                    position: None,
                });
            }
        }
        match result {
            Ok(()) => {
                applied += 1;
                affected.push(1);
            }
            Err(e @ (Error::Connection(_) | Error::Timeout | Error::Cancelled)) => return Err(e),
            Err(e) => return Err(fail(step.index, e.message(), applied)),
        }
    }
    Ok(ApplyResult {
        applied,
        affected,
        atomic: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info_for(types: &[(&str, &str)]) -> Info {
        Info {
            ed: Editability {
                editable: true,
                key_columns: vec!["id".into()],
                read_only_columns: vec!["id".into()],
                reason: None,
                atomic: false,
                notes: vec![EditNote::AsyncMutation],
            },
            types: types
                .iter()
                .map(|(n, t)| (n.to_string(), CType::parse(t).unwrap()))
                .collect(),
            generated: vec!["g".into()],
            key_like: vec!["id".into()],
            table: "`d`.`t`".into(),
            db: "d".into(),
            name: "t".into(),
        }
    }

    fn set(changes: Vec<RowChange>) -> RowChangeSet {
        RowChangeSet {
            table: ObjectPath::root(),
            key_columns: vec!["id".into()],
            changes,
        }
    }

    fn cell(c: &str, v: EditValue) -> CellEdit {
        CellEdit {
            column: c.into(),
            value: v,
        }
    }

    #[test]
    fn engine_families() {
        assert_eq!(engine_mode("MergeTree"), Mode::Full);
        assert_eq!(engine_mode("ReplicatedMergeTree"), Mode::Full);
        assert_eq!(engine_mode("SharedMergeTree"), Mode::Full);
        assert_eq!(engine_mode("ReplacingMergeTree"), Mode::InsertOnly);
        assert_eq!(engine_mode("ReplicatedSummingMergeTree"), Mode::InsertOnly);
        assert_eq!(engine_mode("AggregatingMergeTree"), Mode::InsertOnly);
        assert_eq!(engine_mode("Memory"), Mode::No);
        assert_eq!(engine_mode("Distributed"), Mode::No);
    }

    #[test]
    fn statements_are_rendered_in_order() {
        let info = info_for(&[("id", "UInt32"), ("n", "Nullable(String)")]);
        let rs = set(vec![
            RowChange::Insert {
                values: vec![
                    cell("id", EditValue::Value(Value::Int(3))),
                    cell("n", EditValue::Text("a'b".into())),
                ],
            },
            RowChange::Update {
                key: vec![Value::Int(1)],
                set: vec![cell("n", EditValue::Value(Value::Null))],
            },
            RowChange::Delete {
                key: vec![Value::Int(2)],
            },
        ]);
        let steps: Vec<Step> = [0u8, 1, 2]
            .into_iter()
            .flat_map(|g| {
                rs.changes
                    .iter()
                    .enumerate()
                    .filter(move |(_, c)| {
                        matches!(
                            (g, c),
                            (0, RowChange::Delete { .. })
                                | (1, RowChange::Update { .. })
                                | (2, RowChange::Insert { .. })
                        )
                    })
                    .map(|(i, c)| render_one(&info, &rs, true, i, c).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(steps[0].sql, "DELETE FROM `d`.`t` WHERE `id` = 2");
        assert_eq!(
            steps[0].fallback.as_deref(),
            Some("ALTER TABLE `d`.`t` DELETE WHERE `id` = 2")
        );
        assert_eq!(
            steps[0].guard.as_deref(),
            Some("SELECT count() FROM `d`.`t` WHERE `id` = 2")
        );
        assert_eq!(
            steps[1].sql,
            "ALTER TABLE `d`.`t` UPDATE `n` = NULL WHERE `id` = 1"
        );
        assert_eq!(
            steps[2].sql,
            "INSERT INTO `d`.`t` (`id`, `n`) VALUES (3, 'a\\'b')"
        );
        assert_eq!(steps[2].index, 0);
    }

    #[test]
    fn validation_rules() {
        let info = info_for(&[("id", "UInt32"), ("n", "String"), ("g", "String")]);
        // Update of a key column or generated column is refused.
        let rs = set(vec![RowChange::Update {
            key: vec![Value::Int(1)],
            set: vec![cell("id", EditValue::Value(Value::Int(9)))],
        }]);
        assert!(matches!(
            validate(&info, &rs),
            Err(Error::ChangeFailed { .. })
        ));
        let rs = set(vec![RowChange::Insert {
            values: vec![cell("g", EditValue::Text("x".into()))],
        }]);
        assert!(matches!(
            validate(&info, &rs),
            Err(Error::ChangeFailed { .. })
        ));
        // Inserting the key is fine.
        let rs = set(vec![RowChange::Insert {
            values: vec![cell("id", EditValue::Value(Value::Int(9)))],
        }]);
        assert!(validate(&info, &rs).is_ok());
        // Wrong key columns.
        let mut rs = set(vec![RowChange::Delete {
            key: vec![Value::Int(1)],
        }]);
        rs.key_columns = vec!["n".into()];
        assert!(matches!(validate(&info, &rs), Err(Error::Invalid(_))));
        // Insert-only table.
        let mut io = info_for(&[]);
        io.ed.key_columns = vec![];
        let rs = set(vec![RowChange::Delete {
            key: vec![Value::Int(1)],
        }]);
        assert!(matches!(validate(&io, &rs), Err(Error::Unsupported(_))));
    }

    #[test]
    fn null_key_uses_isnull() {
        let info = info_for(&[("id", "Nullable(UInt32)")]);
        let rs = set(vec![]);
        assert_eq!(
            key_condition(&info, &rs, 0, &[Value::Null]).unwrap(),
            "isNull(`id`)"
        );
        assert!(key_condition(&info, &rs, 0, &[]).is_err());
    }
}
