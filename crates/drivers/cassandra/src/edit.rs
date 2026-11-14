//! Data editing: editability, statement preview and application (lightweight transactions by
//! default).

use dbstudio_core::{
    ApplyResult, CellEdit, ChangeSet, EditNote, EditValue, Editability, Error, NotEditableReason,
    ObjectPath, Result, RowChange, RowChangeSet, Value,
};
use scylla::statement::Statement;

use crate::convert::{literal, literal_from_text};
use crate::core::Core;
use crate::data::applied;
use crate::error;
use crate::ident::{qualified, quote_ident};
use crate::schema::{ColumnInfo, TableInfo};
use crate::tree::parse_path;

fn cell_literal(c: &ColumnInfo, v: &EditValue) -> Result<Option<String>> {
    Ok(match v {
        EditValue::Value(v) => Some(literal(v, &c.ty)?),
        EditValue::Text(s) => Some(literal_from_text(s, &c.ty)?),
        EditValue::Default => None,
    })
}

fn key_condition(t: &TableInfo, key_columns: &[String], key: &[Value]) -> Result<String> {
    let expected: Vec<String> = t.key_columns().iter().map(|c| c.name.clone()).collect();
    if key_columns != expected.as_slice() {
        return Err(Error::Invalid(format!(
            "key columns must be {}",
            expected.join(", ")
        )));
    }
    if key.len() != expected.len() {
        return Err(Error::Invalid(format!(
            "a key needs {} values, got {}",
            expected.len(),
            key.len()
        )));
    }
    let parts = t
        .key_columns()
        .iter()
        .zip(key)
        .map(|(c, v)| Ok(format!("{} = {}", quote_ident(&c.name), literal(v, &c.ty)?)))
        .collect::<Result<Vec<_>>>()?;
    Ok(parts.join(" AND "))
}

fn assignments(t: &TableInfo, set: &[CellEdit]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for edit in set {
        let col = t
            .column(&edit.column)
            .ok_or_else(|| Error::Invalid(format!("unknown column `{}`", edit.column)))?;
        if col.is_key() {
            return Err(Error::Invalid(format!(
                "`{}` is part of the primary key and cannot be edited",
                col.name
            )));
        }
        let lit = cell_literal(col, &edit.value)?.ok_or_else(|| {
            Error::Invalid(format!("`{}`: DEFAULT is not available in CQL", col.name))
        })?;
        out.push((col.name.clone(), lit));
    }
    Ok(out)
}

/// The CQL of one change.
pub(crate) fn statement_for(
    ks: &str,
    t: &TableInfo,
    key_columns: &[String],
    change: &RowChange,
    lwt: bool,
) -> Result<String> {
    let table = qualified(ks, &t.name);
    match change {
        RowChange::Insert { values } => {
            let mut cols = Vec::new();
            let mut lits = Vec::new();
            for edit in values {
                let col = t
                    .column(&edit.column)
                    .ok_or_else(|| Error::Invalid(format!("unknown column `{}`", edit.column)))?;
                if let Some(l) = cell_literal(col, &edit.value)? {
                    cols.push(quote_ident(&col.name));
                    lits.push(l);
                }
            }
            for k in t.key_columns() {
                if !values.iter().any(|v| v.column == k.name) {
                    return Err(Error::Invalid(format!(
                        "the primary key column `{}` needs a value",
                        k.name
                    )));
                }
            }
            Ok(format!(
                "INSERT INTO {table} ({}) VALUES ({}){}",
                cols.join(", "),
                lits.join(", "),
                if lwt { " IF NOT EXISTS" } else { "" }
            ))
        }
        RowChange::Update { key, set } => {
            let sets = assignments(t, set)?;
            if sets.is_empty() {
                return Err(Error::Invalid("an update needs at least one cell".into()));
            }
            Ok(format!(
                "UPDATE {table} SET {} WHERE {}{}",
                sets.iter()
                    .map(|(c, l)| format!("{} = {l}", quote_ident(c)))
                    .collect::<Vec<_>>()
                    .join(", "),
                key_condition(t, key_columns, key)?,
                if lwt { " IF EXISTS" } else { "" }
            ))
        }
        RowChange::Delete { key } => Ok(format!(
            "DELETE FROM {table} WHERE {}{}",
            key_condition(t, key_columns, key)?,
            if lwt { " IF EXISTS" } else { "" }
        )),
    }
}

impl Core {
    pub(crate) async fn editability_of(&self, path: &ObjectPath) -> Result<Editability> {
        let snap = self.snapshot().await?;
        let node = parse_path(path)?;
        let (ks, name, _) = node
            .relation()
            .ok_or_else(|| Error::Invalid("editability needs a table or view".into()))?;
        let t = snap.relation(ks, name)?;
        let keys: Vec<String> = t.key_columns().iter().map(|c| c.name.clone()).collect();
        let reason = if t.is_view {
            Some(NotEditableReason::NotATable)
        } else if t.is_virtual || t.is_counter() {
            Some(NotEditableReason::Unsupported)
        } else if self.pool.settings.read_only {
            Some(NotEditableReason::ReadOnlyConnection)
        } else {
            None
        };
        Ok(Editability {
            editable: reason.is_none(),
            key_columns: keys.clone(),
            read_only_columns: keys,
            reason,
            atomic: false,
            notes: vec![if self.pool.settings.lwt_edits {
                EditNote::ConditionalWrites
            } else {
                EditNote::UpsertSemantics
            }],
        })
    }

    async fn row_statements(&self, set: &RowChangeSet) -> Result<Vec<String>> {
        let snap = self.snapshot().await?;
        let node = parse_path(&set.table)?;
        let (ks, name, _) = node
            .relation()
            .ok_or_else(|| Error::Invalid("the change set needs a table".into()))?;
        let t = snap.relation(ks, name)?;
        if t.is_view || t.is_virtual || t.is_counter() {
            return Err(Error::Unsupported(
                "editing a view, virtual table or counter table".into(),
            ));
        }
        set.changes
            .iter()
            .map(|c| statement_for(ks, t, &set.key_columns, c, self.pool.settings.lwt_edits))
            .collect()
    }

    pub(crate) async fn preview(&self, changes: &ChangeSet) -> Result<Vec<String>> {
        match changes {
            ChangeSet::Rows(set) => self.row_statements(set).await,
            _ => Err(Error::Unsupported("data_editing".into())),
        }
    }

    pub(crate) async fn apply(&self, changes: ChangeSet) -> Result<ApplyResult> {
        let ChangeSet::Rows(set) = changes else {
            return Err(Error::Unsupported("data_editing".into()));
        };
        if self.pool.settings.read_only {
            return Err(Error::ReadOnlyViolation {
                message: "the connection is read-only".into(),
                position: None,
            });
        }
        let statements = self.row_statements(&set).await?;
        let lwt = self.pool.settings.lwt_edits;
        let session = self.pool.base();
        let mut affected = Vec::with_capacity(statements.len());
        for (i, text) in statements.iter().enumerate() {
            let fail = |message: String| Error::ChangeFailed {
                index: i as u32,
                message,
                applied: i as u32,
            };
            let mut stmt = Statement::new(text.clone());
            stmt.set_consistency(self.pool.settings.consistency);
            let result = session
                .query_unpaged(stmt, &[])
                .await
                .map_err(|e| fail(error::from_execution(&e, text).to_string()))?;
            if lwt {
                match applied(result) {
                    Some(true) => affected.push(1),
                    _ => {
                        let msg = match &set.changes[i] {
                            RowChange::Insert { .. } => "a row with this key already exists",
                            _ => "the row no longer exists",
                        };
                        return Err(fail(msg.into()));
                    }
                }
            } else {
                affected.push(1);
            }
        }
        Ok(ApplyResult {
            applied: affected.len() as u32,
            affected,
            atomic: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ColKind;
    use crate::types::CqlType;

    fn table() -> TableInfo {
        let c = |n: &str, k: ColKind, t: &str| ColumnInfo {
            name: n.into(),
            kind: k,
            position: 0,
            descending: false,
            ty: CqlType::parse(t),
        };
        TableInfo {
            name: "t".into(),
            columns: vec![
                c("id", ColKind::Partition, "int"),
                c("ck", ColKind::Clustering, "text"),
                c("v", ColKind::Regular, "text"),
                c("n", ColKind::Regular, "list<int>"),
            ],
            ..TableInfo::default()
        }
    }

    fn keys() -> Vec<String> {
        vec!["id".into(), "ck".into()]
    }

    fn cell(c: &str, v: EditValue) -> CellEdit {
        CellEdit {
            column: c.into(),
            value: v,
        }
    }

    #[test]
    fn insert_update_delete_statements() {
        let t = table();
        let ins = RowChange::Insert {
            values: vec![
                cell("id", EditValue::Value(Value::Int(1))),
                cell("ck", EditValue::Text("a".into())),
                cell("v", EditValue::Text("x'y".into())),
                cell("n", EditValue::Text("[1, 2]".into())),
            ],
        };
        assert_eq!(
            statement_for("ks", &t, &keys(), &ins, true).unwrap(),
            "INSERT INTO ks.t (id, ck, v, n) VALUES (1, 'a', 'x''y', [1, 2]) IF NOT EXISTS"
        );
        assert_eq!(
            statement_for("ks", &t, &keys(), &ins, false).unwrap(),
            "INSERT INTO ks.t (id, ck, v, n) VALUES (1, 'a', 'x''y', [1, 2])"
        );
        let upd = RowChange::Update {
            key: vec![Value::Int(1), Value::Text("a".into())],
            set: vec![cell("v", EditValue::Value(Value::Null))],
        };
        assert_eq!(
            statement_for("ks", &t, &keys(), &upd, true).unwrap(),
            "UPDATE ks.t SET v = null WHERE id = 1 AND ck = 'a' IF EXISTS"
        );
        let del = RowChange::Delete {
            key: vec![Value::Int(1), Value::Text("a".into())],
        };
        assert_eq!(
            statement_for("ks", &t, &keys(), &del, false).unwrap(),
            "DELETE FROM ks.t WHERE id = 1 AND ck = 'a'"
        );
    }

    #[test]
    fn rejects_bad_changes() {
        let t = table();
        let key_edit = RowChange::Update {
            key: vec![Value::Int(1), Value::Text("a".into())],
            set: vec![cell("id", EditValue::Value(Value::Int(2)))],
        };
        assert_eq!(
            statement_for("ks", &t, &keys(), &key_edit, true)
                .unwrap_err()
                .kind(),
            "invalid"
        );
        let missing_key = RowChange::Insert {
            values: vec![cell("id", EditValue::Value(Value::Int(1)))],
        };
        assert!(statement_for("ks", &t, &keys(), &missing_key, true).is_err());
        let wrong_keys = RowChange::Delete {
            key: vec![Value::Int(1)],
        };
        assert!(statement_for("ks", &t, &["id".to_string()], &wrong_keys, true).is_err());
        let bad_value = RowChange::Update {
            key: vec![Value::Int(1), Value::Text("a".into())],
            set: vec![cell("n", EditValue::Text("not a list".into()))],
        };
        assert!(statement_for("ks", &t, &keys(), &bad_value, true).is_err());
        let default = RowChange::Update {
            key: vec![Value::Int(1), Value::Text("a".into())],
            set: vec![cell("v", EditValue::Default)],
        };
        assert!(statement_for("ks", &t, &keys(), &default, true).is_err());
    }
}
