//! MySQL/MariaDB driver: data editing (editability, preview, apply). Owned by work package D.
//!
//! A [`RowChangeSet`] is rendered as one statement per change, ordered deletes first, then
//! updates, then inserts. `apply` runs all statements in a single transaction; every
//! update/delete must match exactly one row. Error indexes always refer to the position of the
//! change in the original `changes` vector.

use dbstudio_core::{
    ApplyResult, CellEdit, ChangeSet, EditValue, Editability, Error, NotEditableReason, ObjectKind,
    ObjectPath, Result, RowChange, RowChangeSet, Session, TableDetails, Value,
};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, OptsBuilder};

use crate::error::{map_connect_error, map_query_error};
use crate::session::MySqlSession;
use crate::sql::{quote_ident, quote_literal, quote_table, table_target};

/// Editability of a table: key = primary key, else the first unique index whose columns are all
/// plain NOT NULL columns. `table_details` reports index columns as DDL fragments (quoted,
/// possibly with a prefix length or `DESC`), so they are decoded here.
pub(crate) async fn editability(session: &MySqlSession, path: &ObjectPath) -> Result<Editability> {
    let not_editable = |reason| Editability {
        editable: false,
        key_columns: vec![],
        read_only_columns: vec![],
        reason: Some(reason),
        atomic: true,
    };
    let d = match session.table_details(path).await {
        Ok(d) => d,
        Err(Error::Unsupported(_)) => return Ok(not_editable(NotEditableReason::Unsupported)),
        Err(e) => return Err(e),
    };
    if d.kind != ObjectKind::Table {
        return Ok(not_editable(NotEditableReason::NotATable));
    }
    let read_only_columns: Vec<String> = d
        .columns
        .iter()
        .filter(|c| is_read_only(c.extra.as_deref()))
        .map(|c| c.name.clone())
        .collect();
    let key_columns = key_columns(&d);
    let editable = !key_columns.is_empty();
    Ok(Editability {
        editable,
        key_columns,
        read_only_columns,
        reason: (!editable).then_some(NotEditableReason::NoUniqueKey),
        atomic: true,
    })
}

/// Generated columns only. `DEFAULT_GENERATED` (a `DEFAULT CURRENT_TIMESTAMP` or expression
/// default) is an ordinary writable column; `table_details` appends ` AS (expr)` to EXTRA when
/// the column has a generation expression.
fn is_read_only(extra: Option<&str>) -> bool {
    extra.is_some_and(|e| {
        let e = e.to_ascii_lowercase();
        e.contains(" as (")
            || e.split_whitespace()
                .any(|w| matches!(w, "virtual" | "stored" | "persistent"))
    })
}

/// Column name of an index fragment such as `` `a` ``, `` `a` DESC `` or `` `a`(10) ``.
/// Returns the name and whether a prefix length was present; expressions yield `None`.
fn index_column(fragment: &str) -> Option<(String, bool)> {
    let rest = fragment.trim().strip_prefix('`')?;
    let mut name = String::new();
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        if c != '`' {
            name.push(c);
            continue;
        }
        let tail = &rest[i + 1..];
        if tail.starts_with('`') {
            name.push('`');
            chars.next();
            continue;
        }
        let tail = tail.trim_end_matches(" DESC");
        return match tail {
            "" => Some((name, false)),
            t if t.starts_with('(') && t.ends_with(')') => Some((name, true)),
            _ => None,
        };
    }
    None
}

fn key_columns(d: &TableDetails) -> Vec<String> {
    if let Some(pk) = d.indexes.iter().find(|i| i.primary) {
        let cols: Option<Vec<String>> = pk
            .columns
            .iter()
            .map(|c| index_column(c).map(|(n, _)| n))
            .collect();
        return cols.unwrap_or_default();
    }
    d.indexes
        .iter()
        .filter(|i| i.unique && !i.columns.is_empty())
        .find_map(|i| {
            let cols: Option<Vec<String>> = i
                .columns
                .iter()
                .map(|c| match index_column(c) {
                    Some((n, false)) => Some(n),
                    _ => None,
                })
                .collect();
            let cols = cols?;
            cols.iter()
                .all(|n| d.columns.iter().any(|c| &c.name == n && !c.nullable))
                .then_some(cols)
        })
        .unwrap_or_default()
}

/// A rendered change plus its position in the original change vector.
struct Step {
    index: usize,
    sql: String,
    /// Updates and deletes must match exactly one row.
    expect_one: bool,
}

/// The statements `apply` would run, in application order.
pub(crate) async fn preview(session: &MySqlSession, changes: &ChangeSet) -> Result<Vec<String>> {
    let rs = rows_of(changes)?;
    let (db, table) = table_target(&rs.table)?;
    let ed = editability(session, &rs.table).await?;
    validate(&ed, rs, false)?;
    let nb = !session.inner.backslash_escapes;
    Ok(render(rs, &quote_table(&db, &table), nb)?
        .into_iter()
        .map(|s| format!("{};", s.sql))
        .collect())
}

/// Apply a change set in one transaction.
pub(crate) async fn apply(session: &MySqlSession, changes: ChangeSet) -> Result<ApplyResult> {
    let rs = rows_of(&changes)?;
    let (db, table) = table_target(&rs.table)?;
    let ed = editability(session, &rs.table).await?;
    validate(&ed, rs, true)?;
    // A dedicated connection with CLIENT_FOUND_ROWS: the affected count of an UPDATE is then the
    // number of matched rows (also when the values did not change), without parsing server text.
    let opts = OptsBuilder::from_opts(session.inner.opts.clone()).client_found_rows(true);
    let mut conn = Conn::new(opts)
        .await
        .map_err(|e| map_connect_error(e, None))?;
    let r = run(&mut conn, rs, &quote_table(&db, &table)).await;
    let _ = conn.disconnect().await;
    r
}

async fn run(conn: &mut Conn, rs: &RowChangeSet, table: &str) -> Result<ApplyResult> {
    let mode: Option<String> = conn
        .query_first("SELECT @@SESSION.sql_mode")
        .await
        .map_err(map_query_error)?;
    let nb = mode.is_some_and(|m| m.to_ascii_uppercase().contains("NO_BACKSLASH_ESCAPES"));
    let steps = render(rs, table, nb)?;
    if steps.is_empty() {
        return Ok(ApplyResult {
            applied: 0,
            affected: vec![],
            atomic: true,
        });
    }
    conn.query_drop("START TRANSACTION")
        .await
        .map_err(map_query_error)?;
    let mut affected = Vec::with_capacity(steps.len());
    for step in &steps {
        if let Err(e) = conn.query_drop(step.sql.as_str()).await {
            let err = map_query_error(e);
            return Err(abort(conn, step.index, &err.message(), err).await);
        }
        let n = conn.affected_rows();
        if step.expect_one && n != 1 {
            let msg = format!("expected exactly 1 affected row, got {n}");
            return Err(abort(conn, step.index, &msg, Error::Other(msg.clone())).await);
        }
        affected.push(n);
    }
    if let Err(e) = conn.query_drop("COMMIT").await {
        // The failure belongs to no single change: report it without an index.
        let err = map_query_error(e);
        let _ = conn.query_drop("ROLLBACK").await;
        return Err(err);
    }
    Ok(ApplyResult {
        applied: steps.len() as u32,
        affected,
        atomic: true,
    })
}

/// Roll back and build the error. Connection-level failures are returned unchanged (the server
/// rolls an abandoned transaction back by itself).
async fn abort(conn: &mut Conn, index: usize, message: &str, cause: Error) -> Error {
    if matches!(cause, Error::Connection(_) | Error::Timeout) {
        return cause;
    }
    if conn.query_drop("ROLLBACK").await.is_err() {
        return Error::Connection("rollback failed".into());
    }
    Error::ChangeFailed {
        index: index as u32,
        message: message.to_string(),
        applied: 0,
    }
}

/// Reject change sets that do not match the table: wrong key columns or read-only columns.
/// `strict_table` also demands an editable table (apply, or any preview with updates/deletes).
fn validate(ed: &Editability, rs: &RowChangeSet, strict_table: bool) -> Result<()> {
    let keyed = rs
        .changes
        .iter()
        .any(|c| matches!(c, RowChange::Update { .. } | RowChange::Delete { .. }));
    if strict_table || keyed {
        if !ed.editable {
            return Err(Error::Unsupported(format!(
                "table is not editable ({})",
                ed.reason
                    .map_or("unknown".to_string(), |r| format!("{r:?}"))
            )));
        }
        if keyed && rs.key_columns != ed.key_columns {
            return Err(Error::Other(format!(
                "key columns {:?} do not match the table's key {:?}",
                rs.key_columns, ed.key_columns
            )));
        }
    }
    for (i, change) in rs.changes.iter().enumerate() {
        let (cells, insert): (&[CellEdit], bool) = match change {
            RowChange::Update { set, .. } => (set, false),
            RowChange::Insert { values } => (values, true),
            RowChange::Delete { .. } => continue,
        };
        for c in cells {
            let default_on_insert = insert && matches!(c.value, EditValue::Default);
            if ed.read_only_columns.contains(&c.column) && !default_on_insert {
                return Err(bad(i, format!("column {} is read-only", c.column)));
            }
        }
    }
    Ok(())
}

fn rows_of(changes: &ChangeSet) -> Result<&RowChangeSet> {
    match changes {
        ChangeSet::Rows(rs) => Ok(rs),
        _ => Err(Error::Unsupported(
            "this connection only edits table rows".into(),
        )),
    }
}

fn bad(index: usize, message: impl Into<String>) -> Error {
    Error::ChangeFailed {
        index: index as u32,
        message: message.into(),
        applied: 0,
    }
}

/// Render every change, ordered deletes, updates, inserts (stable within each group).
fn render(rs: &RowChangeSet, table: &str, nb: bool) -> Result<Vec<Step>> {
    let mut steps = Vec::with_capacity(rs.changes.len());
    for want in [0u8, 1, 2] {
        for (index, change) in rs.changes.iter().enumerate() {
            let group = match change {
                RowChange::Delete { .. } => 0,
                RowChange::Update { .. } => 1,
                RowChange::Insert { .. } => 2,
            };
            if group == want {
                steps.push(render_one(rs, table, nb, index, change)?);
            }
        }
    }
    Ok(steps)
}

fn render_one(
    rs: &RowChangeSet,
    table: &str,
    nb: bool,
    index: usize,
    change: &RowChange,
) -> Result<Step> {
    match change {
        RowChange::Update { key, set } => {
            if set.is_empty() {
                return Err(bad(index, "update without any cell"));
            }
            let assigns = set
                .iter()
                .map(|c| {
                    Ok(format!(
                        "{} = {}",
                        quote_ident(&c.column),
                        edit_literal(index, &c.value, nb)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Step {
                index,
                sql: format!(
                    "UPDATE {table} SET {} WHERE {}",
                    assigns.join(", "),
                    where_clause(rs, index, key, nb)?
                ),
                expect_one: true,
            })
        }
        RowChange::Delete { key } => Ok(Step {
            index,
            sql: format!(
                "DELETE FROM {table} WHERE {}",
                where_clause(rs, index, key, nb)?
            ),
            expect_one: true,
        }),
        RowChange::Insert { values } => {
            let sql = if values.is_empty() {
                format!("INSERT INTO {table} () VALUES ()")
            } else {
                let cols: Vec<String> = values.iter().map(|c| quote_ident(&c.column)).collect();
                let vals = values
                    .iter()
                    .map(|c| edit_literal(index, &c.value, nb))
                    .collect::<Result<Vec<_>>>()?;
                format!(
                    "INSERT INTO {table} ({}) VALUES ({})",
                    cols.join(", "),
                    vals.join(", ")
                )
            };
            Ok(Step {
                index,
                sql,
                expect_one: false,
            })
        }
    }
}

fn where_clause(rs: &RowChangeSet, index: usize, key: &[Value], nb: bool) -> Result<String> {
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
                Value::Null => format!("{col} IS NULL"),
                v => format!("{col} = {}", literal(index, v, nb)?),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(parts.join(" AND "))
}

fn edit_literal(index: usize, v: &EditValue, nb: bool) -> Result<String> {
    match v {
        EditValue::Default => Ok("DEFAULT".into()),
        EditValue::Text(s) => Ok(quote_literal(s, nb)),
        EditValue::Value(v) => literal(index, v, nb),
    }
}

/// SQL literal of a value for the session's `NO_BACKSLASH_ESCAPES` mode `nb`.
fn literal(index: usize, v: &Value, nb: bool) -> Result<String> {
    Ok(match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) if f.is_finite() => format!("{f:?}"),
        Value::Float(f) => quote_literal(&f.to_string(), nb),
        Value::Decimal(s) if s.parse::<f64>().is_ok_and(f64::is_finite) => s.clone(),
        Value::Decimal(s) => quote_literal(s, nb),
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            format!("X'{hex}'")
        }
        Value::Text(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::Timestamp(s)
        | Value::Uuid(s)
        | Value::Json(s) => quote_literal(s, nb),
        Value::Ext(e) => quote_literal(&e.text, nb),
        Value::Array(_) | Value::Document(_) => {
            return Err(bad(index, "arrays and documents must be typed as text"));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cell(c: &str, v: EditValue) -> CellEdit {
        CellEdit {
            column: c.into(),
            value: v,
        }
    }

    fn set(changes: Vec<RowChange>) -> RowChangeSet {
        RowChangeSet {
            table: ObjectPath::root()
                .child(ObjectKind::Database, "d")
                .child(ObjectKind::Table, "t"),
            key_columns: vec!["id".into()],
            changes,
        }
    }

    #[test]
    fn order_is_delete_update_insert_and_keeps_indexes() {
        let rs = set(vec![
            RowChange::Insert {
                values: vec![cell("n", EditValue::Text("a'b\\".into()))],
            },
            RowChange::Update {
                key: vec![Value::Int(1)],
                set: vec![
                    cell("n", EditValue::Value(Value::Null)),
                    cell("m", EditValue::Text(String::new())),
                ],
            },
            RowChange::Delete {
                key: vec![Value::Int(2)],
            },
        ]);
        let steps = render(&rs, "`d`.`t`", false).unwrap();
        let idx: Vec<usize> = steps.iter().map(|s| s.index).collect();
        assert_eq!(idx, vec![2, 1, 0]);
        assert_eq!(steps[0].sql, "DELETE FROM `d`.`t` WHERE `id` = 2");
        assert_eq!(
            steps[1].sql,
            "UPDATE `d`.`t` SET `n` = NULL, `m` = '' WHERE `id` = 1"
        );
        assert_eq!(
            steps[2].sql,
            "INSERT INTO `d`.`t` (`n`) VALUES ('a''b\\\\')"
        );
        // NO_BACKSLASH_ESCAPES keeps the backslash as is.
        let steps = render(&rs, "t", true).unwrap();
        assert_eq!(steps[2].sql, "INSERT INTO t (`n`) VALUES ('a''b\\')");
    }

    #[test]
    fn empty_insert_and_bad_keys() {
        let rs = set(vec![RowChange::Insert { values: vec![] }]);
        assert_eq!(
            render(&rs, "t", false).unwrap()[0].sql,
            "INSERT INTO t () VALUES ()"
        );
        let rs = set(vec![
            RowChange::Delete {
                key: vec![Value::Int(1)],
            },
            RowChange::Delete { key: vec![] },
        ]);
        match render(&rs, "t", false) {
            Err(Error::ChangeFailed { index, .. }) => assert_eq!(index, 1),
            other => panic!("{:?}", other.err()),
        }
    }

    #[test]
    fn index_fragments_are_decoded() {
        assert_eq!(index_column("`id`"), Some(("id".into(), false)));
        assert_eq!(index_column("`a``b` DESC"), Some(("a`b".into(), false)));
        assert_eq!(index_column("`t`(10)"), Some(("t".into(), true)));
        assert_eq!(index_column("(lower(`x`))"), None);
        assert!(is_read_only(Some("VIRTUAL GENERATED")));
        assert!(is_read_only(Some("STORED GENERATED")));
        assert!(is_read_only(Some(" AS (`n` * 2)")));
        assert!(!is_read_only(Some("DEFAULT_GENERATED")));
        assert!(!is_read_only(Some(
            "DEFAULT_GENERATED on update CURRENT_TIMESTAMP"
        )));
        assert!(!is_read_only(Some("auto_increment")));
    }

    #[test]
    fn bytes_bools_and_default() {
        let rs = set(vec![RowChange::Insert {
            values: vec![
                cell("b", EditValue::Value(Value::Bytes(vec![0, 255]))),
                cell("f", EditValue::Value(Value::Bool(true))),
                cell("d", EditValue::Default),
            ],
        }]);
        assert_eq!(
            render(&rs, "t", false).unwrap()[0].sql,
            "INSERT INTO t (`b`, `f`, `d`) VALUES (X'00ff', TRUE, DEFAULT)"
        );
    }
}
