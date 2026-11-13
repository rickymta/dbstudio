//! Data editing: editability, preview and atomic apply of a [`ChangeSet`].

use dbstudio_core::{
    ApplyResult, CellEdit, ChangeSet, CollectingSink, EditValue, Editability, Error,
    NotEditableReason, ObjectKind, ObjectPath, Result, RowChange, RowChangeSet, StatementOutcome,
    Value,
};

use crate::catalog;
use crate::conn::Conn;
use crate::exec::{Control, RunParams, SinkOut, run_script};
use crate::session::MssqlSession;
use crate::sql::{ObjRef, lit, parse_object, quote, value_lit};
use crate::value::str_at;

/// Editability of the table at `path`.
pub async fn editability(sess: &MssqlSession, path: &ObjectPath) -> Result<Editability> {
    let obj = parse_object(path)?;
    let db = obj
        .db
        .clone()
        .unwrap_or_else(|| sess.login_db().to_string());
    let not_editable = |reason| Editability {
        editable: false,
        key_columns: Vec::new(),
        read_only_columns: Vec::new(),
        reason: Some(reason),
        atomic: true,
    };
    if !matches!(obj.kind, ObjectKind::Table | ObjectKind::View) {
        return Ok(not_editable(NotEditableReason::NotATable));
    }
    let mut conn = sess.shared.acquire_catalog(&db).await?;
    let filter = catalog::one(&obj.schema, &obj.name);
    let kind_rows = conn
        .rows(&format!(
            "SELECT type FROM sys.objects WHERE object_id = OBJECT_ID({})",
            lit(&obj.qualified())
        ))
        .await;
    let kind_rows = match kind_rows {
        Ok(r) => r,
        Err(e) => {
            conn.release();
            return Err(e);
        }
    };
    let Some(kind_row) = kind_rows.first() else {
        conn.release();
        return Err(Error::NotFound(format!("table {}", obj.qualified())));
    };
    if str_at(kind_row, 0).trim() != "U" {
        conn.release();
        return Ok(not_editable(NotEditableReason::NotATable));
    }
    let key = crate::data::key_columns(&mut conn, &obj).await;
    let cols = catalog::columns(&mut conn, &filter).await;
    conn.release();
    let key = key?;
    let cols = cols?
        .remove(&(obj.schema.clone(), obj.name.clone()))
        .unwrap_or_default();
    let read_only_columns: Vec<String> = cols
        .iter()
        .filter(|c| {
            c.identity.is_some() || c.computed.is_some() || c.rowversion || c.generated != 0
        })
        .map(|c| c.name.clone())
        .collect();
    if sess.read_only {
        return Ok(Editability {
            editable: false,
            key_columns: key,
            read_only_columns,
            reason: Some(NotEditableReason::ReadOnlyConnection),
            atomic: true,
        });
    }
    if key.is_empty() {
        return Ok(Editability {
            editable: false,
            key_columns: key,
            read_only_columns,
            reason: Some(NotEditableReason::NoUniqueKey),
            atomic: true,
        });
    }
    Ok(Editability {
        editable: true,
        key_columns: key,
        read_only_columns,
        reason: None,
        atomic: true,
    })
}

fn edit_lit(v: &EditValue) -> String {
    match v {
        EditValue::Value(v) => value_lit(v),
        EditValue::Text(t) => lit(t),
        EditValue::Default => "DEFAULT".to_string(),
    }
}

fn key_condition(cols: &[String], key: &[Value]) -> Result<String> {
    if cols.is_empty() {
        return Err(Error::Other("the change set has no key columns".into()));
    }
    if cols.len() != key.len() {
        return Err(Error::Other(format!(
            "key has {} values but {} key columns were given",
            key.len(),
            cols.len()
        )));
    }
    Ok(cols
        .iter()
        .zip(key)
        .map(|(c, v)| match v {
            Value::Null => format!("{} IS NULL", quote(c)),
            v => format!("{} = {}", quote(c), value_lit(v)),
        })
        .collect::<Vec<_>>()
        .join(" AND "))
}

fn assignments(set: &[CellEdit]) -> String {
    set.iter()
        .map(|c| format!("{} = {}", quote(&c.column), edit_lit(&c.value)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One statement per change, in application order.
pub fn statements(cs: &RowChangeSet) -> Result<Vec<String>> {
    let obj = parse_object(&cs.table)?;
    let table = obj.qualified();
    cs.changes
        .iter()
        .map(|ch| match ch {
            RowChange::Update { key, set } => {
                if set.is_empty() {
                    return Err(Error::Other("an update without cells".into()));
                }
                Ok(format!(
                    "UPDATE {table} SET {} WHERE {};",
                    assignments(set),
                    key_condition(&cs.key_columns, key)?
                ))
            }
            RowChange::Insert { values } => {
                if values.is_empty() {
                    return Ok(format!("INSERT INTO {table} DEFAULT VALUES;"));
                }
                let cols: Vec<String> = values.iter().map(|c| quote(&c.column)).collect();
                let vals: Vec<String> = values.iter().map(|c| edit_lit(&c.value)).collect();
                Ok(format!(
                    "INSERT INTO {table} ({}) VALUES ({});",
                    cols.join(", "),
                    vals.join(", ")
                ))
            }
            RowChange::Delete { key } => Ok(format!(
                "DELETE FROM {table} WHERE {};",
                key_condition(&cs.key_columns, key)?
            )),
        })
        .collect()
}

fn rows_changes(cs: &ChangeSet) -> Result<&RowChangeSet> {
    match cs {
        ChangeSet::Rows(r) => Ok(r),
        _ => Err(Error::Unsupported("data_editing".into())),
    }
}

pub async fn preview(_sess: &MssqlSession, changes: &ChangeSet) -> Result<Vec<String>> {
    statements(rows_changes(changes)?)
}

/// Affected rows of the outer statement: its `DONE` is the last count the server sends (triggers
/// report theirs before it).
async fn run_change(conn: &mut Conn, sql: &str) -> Result<u64> {
    let mut sink = CollectingSink::new();
    let ctl = Control::default();
    let params = RunParams {
        max_rows: None,
        batch_size: 100,
        timeout: None,
    };
    run_script(conn, sql, &params, &ctl, None, &mut SinkOut(&mut sink)).await?;
    Ok(sink
        .into_results()
        .iter()
        .rev()
        .find_map(|r| match r.outcome {
            StatementOutcome::Affected { rows } => Some(rows),
            _ => None,
        })
        .unwrap_or(0))
}

pub async fn apply(sess: &MssqlSession, changes: ChangeSet) -> Result<ApplyResult> {
    if sess.read_only {
        return Err(Error::ReadOnlyViolation {
            message: "the connection is read-only".into(),
            position: None,
        });
    }
    let cs = rows_changes(&changes)?;
    let stmts = statements(cs)?;
    let obj: ObjRef = parse_object(&cs.table)?;
    let db = obj
        .db
        .clone()
        .unwrap_or_else(|| sess.login_db().to_string());
    let mut conn = sess.shared.acquire(&db).await?;
    // The client names the key; it must be a real primary or unique key of the table.
    if cs
        .changes
        .iter()
        .any(|c| !matches!(c, RowChange::Insert { .. }))
    {
        let candidates = match crate::data::candidate_keys(&mut conn, &obj).await {
            Ok(c) => c,
            Err(e) => {
                conn.release();
                return Err(e);
            }
        };
        let same = |a: &[String], b: &[String]| {
            a.len() == b.len()
                && a.iter()
                    .all(|x| b.iter().any(|y| y.eq_ignore_ascii_case(x)))
        };
        if !candidates.iter().any(|k| same(k, &cs.key_columns)) {
            conn.release();
            return Err(Error::Other(
                "key_columns do not match a primary or unique NOT NULL key of the table".into(),
            ));
        }
    }
    conn.run("BEGIN TRANSACTION").await?;
    let mut affected = Vec::with_capacity(stmts.len());
    for (index, sql) in stmts.iter().enumerate() {
        let outcome = run_change(&mut conn, sql).await;
        let failure = match (&cs.changes[index], outcome) {
            (_, Err(e)) => Some(e.to_string()),
            (RowChange::Update { .. } | RowChange::Delete { .. }, Ok(0)) => Some(
                "no row matched the key (0 rows affected); it may have been changed or deleted"
                    .to_string(),
            ),
            (RowChange::Update { .. } | RowChange::Delete { .. }, Ok(n)) if n != 1 => Some(
                format!("the key matched {n} rows instead of one; nothing was changed"),
            ),
            (_, Ok(n)) => {
                affected.push(n);
                None
            }
        };
        if let Some(message) = failure {
            if conn
                .run("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION")
                .await
                .is_err()
            {
                conn.broken = true;
            }
            if !conn.broken {
                conn.release();
            }
            return Err(Error::ChangeFailed {
                index: index as u32,
                message,
                applied: 0,
            });
        }
    }
    if let Err(e) = conn.run("COMMIT TRANSACTION").await {
        let _ = conn.run("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await;
        conn.broken = true;
        return Err(Error::ChangeFailed {
            index: stmts.len().saturating_sub(1) as u32,
            message: e.to_string(),
            applied: 0,
        });
    }
    conn.release();
    // Schema-dependent caches (key columns) stay valid: only rows changed.
    Ok(ApplyResult {
        applied: stmts.len() as u32,
        affected,
        atomic: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{ObjectKind, ObjectPath};

    fn set(table_cols: &[&str], changes: Vec<RowChange>) -> RowChangeSet {
        RowChangeSet {
            table: ObjectPath::root()
                .child(ObjectKind::Database, "d")
                .child(ObjectKind::Schema, "s")
                .child(ObjectKind::Table, "t"),
            key_columns: table_cols.iter().map(|s| s.to_string()).collect(),
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
    fn statements_in_order() {
        let cs = set(
            &["id", "line"],
            vec![
                RowChange::Update {
                    key: vec![Value::Int(1), Value::Int(2)],
                    set: vec![
                        cell("name", EditValue::Text("O'Hara".into())),
                        cell("n", EditValue::Value(Value::Null)),
                        cell("d", EditValue::Default),
                        cell("b", EditValue::Value(Value::Bool(true))),
                    ],
                },
                RowChange::Insert {
                    values: vec![cell("x", EditValue::Value(Value::Decimal("1.50".into())))],
                },
                RowChange::Insert { values: vec![] },
                RowChange::Delete {
                    key: vec![Value::Text("a".into()), Value::Null],
                },
            ],
        );
        let s = statements(&cs).unwrap();
        assert_eq!(
            s[0],
            "UPDATE [s].[t] SET [name] = N'O''Hara', [n] = NULL, [d] = DEFAULT, [b] = 1 WHERE [id] = 1 AND [line] = 2;"
        );
        assert_eq!(s[1], "INSERT INTO [s].[t] ([x]) VALUES (1.50);");
        assert_eq!(s[2], "INSERT INTO [s].[t] DEFAULT VALUES;");
        assert_eq!(
            s[3],
            "DELETE FROM [s].[t] WHERE [id] = N'a' AND [line] IS NULL;"
        );
    }

    #[test]
    fn key_problems_are_errors() {
        let cs = set(&["id"], vec![RowChange::Delete { key: vec![] }]);
        assert!(statements(&cs).is_err());
        let cs = set(
            &[],
            vec![RowChange::Delete {
                key: vec![Value::Int(1)],
            }],
        );
        assert!(statements(&cs).is_err());
        let cs = set(
            &["id"],
            vec![RowChange::Update {
                key: vec![Value::Int(1)],
                set: vec![],
            }],
        );
        assert!(statements(&cs).is_err());
    }
}
