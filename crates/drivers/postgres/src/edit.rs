//! PostgreSQL driver: data editing (editability, preview, apply). Owned by work package D.
//!
//! A [`RowChangeSet`] is rendered as one statement per change, ordered deletes first, then
//! updates, then inserts (fewer unique-key conflicts). `apply` runs all statements in a single
//! transaction; every update/delete must touch exactly one row. Error indexes always refer to the
//! position of the change in the original `changes` vector.

use dbstudio_core::{
    ApplyResult, CellEdit, ChangeSet, EditValue, Editability, Error, NotEditableReason, ObjectKind,
    ObjectPath, Result, RowChange, RowChangeSet, Value,
};

use crate::details;
use crate::error::map_error;
use crate::session::PostgresSession;
use crate::sql::{qualified, quote_ident, quote_literal};

/// Editability of a table. Key columns come straight from `pg_attribute` (plain names: the index
/// definitions of `table_details` are quoted, which would break `"UserId"`-style columns) and
/// only from full, valid, non-partial, non-expression indexes.
pub(crate) async fn editability(
    session: &PostgresSession,
    path: &ObjectPath,
) -> Result<Editability> {
    let not_editable = |reason| Editability {
        editable: false,
        key_columns: vec![],
        read_only_columns: vec![],
        reason: Some(reason),
        atomic: true,
        notes: vec![],
    };
    let rel = details::resolve_relation(path)?;
    if rel.kind != ObjectKind::Table {
        return Ok(not_editable(NotEditableReason::NotATable));
    }
    let mut conn = session.shared.acquire_catalog(&rel.database).await?;
    let d =
        details::table_details(&conn.client, path, &rel.database, session.shared.flavor()).await;
    let keys = match &d {
        Ok(_) => details::key_columns(&conn.client, &rel.schema, &rel.name).await,
        Err(_) => Ok(vec![]),
    };
    if d.is_ok() && keys.is_ok() {
        conn.finish().await;
    }
    let (d, key_columns) = (d?, keys?);
    // A table without primary key is keyed on CockroachDB's hidden `rowid`: it cannot be edited.
    let hidden_rowid = session.shared.flavor().is_cockroach()
        && key_columns == ["rowid"]
        && !d.columns.iter().any(|c| c.name == "rowid");
    let read_only_columns = d
        .columns
        .iter()
        .filter(|c| {
            c.extra.as_deref().is_some_and(|e| {
                let e = e.to_ascii_lowercase();
                !e.contains("by default") && (e.contains("identity") || e.contains("generated"))
            })
        })
        .map(|c| c.name.clone())
        .chain(hidden_rowid.then(|| "rowid".to_string()))
        .collect();
    let editable = !key_columns.is_empty();
    Ok(Editability {
        editable,
        key_columns,
        read_only_columns,
        reason: (!editable).then_some(NotEditableReason::NoUniqueKey),
        atomic: true,
        notes: vec![],
    })
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

/// A rendered change plus its position in the original change vector.
struct Step {
    index: usize,
    sql: String,
    /// Updates and deletes must affect exactly one row.
    expect_one: bool,
}

/// The statements `apply` would run, in application order.
pub(crate) async fn preview(session: &PostgresSession, changes: &ChangeSet) -> Result<Vec<String>> {
    let rs = rows_of(changes)?;
    let table = resolve_table(rs)?;
    let ed = editability(session, &rs.table).await?;
    validate(&ed, rs, false)?;
    Ok(render(rs, &table)?
        .into_iter()
        .map(|s| format!("{};", s.sql))
        .collect())
}

/// Apply a change set in one transaction.
pub(crate) async fn apply(session: &PostgresSession, changes: ChangeSet) -> Result<ApplyResult> {
    let rs = rows_of(&changes)?;
    let rel = details::resolve_relation(&rs.table)?;
    let table = qualified(&rel.schema, &rel.name);
    let ed = editability(session, &rs.table).await?;
    validate(&ed, rs, true)?;
    let steps = render(rs, &table)?;
    if steps.is_empty() {
        return Ok(ApplyResult {
            applied: 0,
            affected: vec![],
            atomic: true,
        });
    }
    let mut conn = session.shared.acquire(&rel.database).await?;
    if let Err(e) = conn.client.batch_execute("BEGIN").await {
        return Err(map_error(e));
    }
    conn.in_txn = true;
    let mut affected = Vec::with_capacity(steps.len());
    for step in &steps {
        let n = match conn.client.execute(step.sql.as_str(), &[]).await {
            Ok(n) => n,
            Err(e) => {
                let err = map_error(e);
                if conn.client.is_closed() {
                    return Err(err);
                }
                conn.finish().await; // ROLLBACK
                return Err(failed(step.index, &err));
            }
        };
        if step.expect_one && n != 1 {
            conn.finish().await;
            return Err(Error::ChangeFailed {
                index: step.index as u32,
                message: format!("expected exactly 1 affected row, got {n}"),
                applied: 0,
            });
        }
        affected.push(n);
    }
    if let Err(e) = conn.client.batch_execute("COMMIT").await {
        let err = map_error(e);
        if conn.client.is_closed() {
            return Err(err);
        }
        conn.finish().await;
        // The failure belongs to no single change (deferred constraint): no index.
        return Err(err);
    }
    conn.in_txn = false;
    conn.finish().await;
    Ok(ApplyResult {
        applied: steps.len() as u32,
        affected,
        atomic: true,
    })
}

fn failed(index: usize, e: &Error) -> Error {
    Error::ChangeFailed {
        index: index as u32,
        message: e.message(),
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

fn resolve_table(rs: &RowChangeSet) -> Result<String> {
    let rel = details::resolve_relation(&rs.table)?;
    Ok(qualified(&rel.schema, &rel.name))
}

/// Render every change, ordered deletes, updates, inserts (stable within each group).
fn render(rs: &RowChangeSet, table: &str) -> Result<Vec<Step>> {
    let mut steps = Vec::with_capacity(rs.changes.len());
    for want in [0u8, 1, 2] {
        for (index, change) in rs.changes.iter().enumerate() {
            let group = match change {
                RowChange::Delete { .. } => 0,
                RowChange::Update { .. } => 1,
                RowChange::Insert { .. } => 2,
            };
            if group != want {
                continue;
            }
            steps.push(render_one(rs, table, index, change)?);
        }
    }
    Ok(steps)
}

fn bad(index: usize, message: impl Into<String>) -> Error {
    Error::ChangeFailed {
        index: index as u32,
        message: message.into(),
        applied: 0,
    }
}

fn render_one(rs: &RowChangeSet, table: &str, index: usize, change: &RowChange) -> Result<Step> {
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
                        edit_literal(index, &c.value)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Step {
                index,
                sql: format!(
                    "UPDATE {table} SET {} WHERE {}",
                    assigns.join(", "),
                    where_clause(rs, index, key)?
                ),
                expect_one: true,
            })
        }
        RowChange::Delete { key } => Ok(Step {
            index,
            sql: format!(
                "DELETE FROM {table} WHERE {}",
                where_clause(rs, index, key)?
            ),
            expect_one: true,
        }),
        RowChange::Insert { values } => {
            let sql = if values.is_empty() {
                format!("INSERT INTO {table} DEFAULT VALUES")
            } else {
                let cols: Vec<String> = values.iter().map(|c| quote_ident(&c.column)).collect();
                let vals = values
                    .iter()
                    .map(|c| edit_literal(index, &c.value))
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

fn where_clause(rs: &RowChangeSet, index: usize, key: &[Value]) -> Result<String> {
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
                v => format!("{col} = {}", literal(index, v)?),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(parts.join(" AND "))
}

fn edit_literal(index: usize, v: &EditValue) -> Result<String> {
    match v {
        EditValue::Default => Ok("DEFAULT".into()),
        EditValue::Text(s) => Ok(quote_literal(s)),
        EditValue::Value(v) => literal(index, v),
    }
}

/// SQL literal of a value; independent of `standard_conforming_strings`.
fn literal(index: usize, v: &Value) -> Result<String> {
    Ok(match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) if f.is_finite() => format!("{f:?}"),
        Value::Float(f) => quote_literal(&f.to_string()),
        Value::Decimal(s) if s.parse::<f64>().is_ok_and(f64::is_finite) => s.clone(),
        Value::Decimal(s) => quote_literal(s),
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            format!("decode('{hex}', 'hex')")
        }
        Value::Text(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::Timestamp(s)
        | Value::Uuid(s)
        | Value::Json(s) => quote_literal(s),
        Value::Ext(e) => quote_literal(&e.text),
        Value::Array(items) => quote_literal(&array_text(index, items)?),
        Value::Document(_) => {
            return Err(bad(index, "documents must be typed as text for PostgreSQL"));
        }
    })
}

/// PostgreSQL array text form `{1,"a b",NULL}`; the server coerces it to the column's type.
fn array_text(index: usize, items: &[Value]) -> Result<String> {
    let mut parts = Vec::with_capacity(items.len());
    for v in items {
        parts.push(match v {
            Value::Null => "NULL".to_string(),
            Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Value::Array(inner) => array_text(index, inner)?,
            Value::Document(_) | Value::Bytes(_) => {
                return Err(bad(index, "unsupported array element"));
            }
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Decimal(t)
            | Value::Text(t)
            | Value::Date(t)
            | Value::Time(t)
            | Value::DateTime(t)
            | Value::Timestamp(t)
            | Value::Uuid(t)
            | Value::Json(t) => format!("\"{}\"", t.replace('\\', "\\\\").replace('"', "\\\"")),
            Value::Ext(e) => format!("\"{}\"", e.text.replace('\\', "\\\\").replace('"', "\\\"")),
        });
    }
    Ok(format!("{{{}}}", parts.join(",")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::ObjectKind;

    fn table() -> ObjectPath {
        ObjectPath::root()
            .child(ObjectKind::Database, "db")
            .child(ObjectKind::Schema, "public")
            .child(ObjectKind::Table, "t")
    }

    fn cell(c: &str, v: EditValue) -> CellEdit {
        CellEdit {
            column: c.into(),
            value: v,
        }
    }

    fn set(changes: Vec<RowChange>) -> RowChangeSet {
        RowChangeSet {
            table: table(),
            key_columns: vec!["id".into()],
            changes,
        }
    }

    #[test]
    fn order_is_delete_update_insert_and_keeps_indexes() {
        let rs = set(vec![
            RowChange::Insert {
                values: vec![cell("n", EditValue::Text("a'b".into()))],
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
        let steps = render(&rs, "\"public\".\"t\"").unwrap();
        let idx: Vec<usize> = steps.iter().map(|s| s.index).collect();
        assert_eq!(idx, vec![2, 1, 0]);
        assert_eq!(
            steps[0].sql,
            "DELETE FROM \"public\".\"t\" WHERE \"id\" = 2"
        );
        assert_eq!(
            steps[1].sql,
            "UPDATE \"public\".\"t\" SET \"n\" = NULL, \"m\" = '' WHERE \"id\" = 1"
        );
        assert_eq!(
            steps[2].sql,
            "INSERT INTO \"public\".\"t\" (\"n\") VALUES ('a''b')"
        );
    }

    #[test]
    fn literals_and_composite_keys() {
        let mut rs = set(vec![RowChange::Delete {
            key: vec![Value::Text("x\\y".into()), Value::Null],
        }]);
        rs.key_columns = vec!["a".into(), "b".into()];
        let s = render(&rs, "t").unwrap();
        assert_eq!(
            s[0].sql,
            "DELETE FROM t WHERE \"a\" = E'x\\\\y' AND \"b\" IS NULL"
        );
        let rs = set(vec![RowChange::Insert { values: vec![] }]);
        assert_eq!(
            render(&rs, "t").unwrap()[0].sql,
            "INSERT INTO t DEFAULT VALUES"
        );
        let rs = set(vec![RowChange::Insert {
            values: vec![
                cell("d", EditValue::Default),
                cell("b", EditValue::Value(Value::Bytes(vec![0, 255]))),
            ],
        }]);
        assert_eq!(
            render(&rs, "t").unwrap()[0].sql,
            "INSERT INTO t (\"d\", \"b\") VALUES (DEFAULT, decode('00ff', 'hex'))"
        );
    }

    #[test]
    fn arrays_render_as_array_text() {
        let v = Value::Array(vec![
            Value::Int(1),
            Value::Null,
            Value::Text("a \"b\" \\c".into()),
            Value::Array(vec![Value::Bool(true)]),
        ]);
        assert_eq!(
            literal(0, &v).unwrap(),
            "E'{1,NULL,\"a \\\\\"b\\\\\" \\\\\\\\c\",{true}}'"
        );
    }

    #[test]
    fn bad_keys_report_the_change_index() {
        let rs = set(vec![
            RowChange::Delete {
                key: vec![Value::Int(1)],
            },
            RowChange::Delete { key: vec![] },
        ]);
        match render(&rs, "t") {
            Err(Error::ChangeFailed { index, .. }) => assert_eq!(index, 1),
            other => panic!("{:?}", other.err()),
        }
    }
}
