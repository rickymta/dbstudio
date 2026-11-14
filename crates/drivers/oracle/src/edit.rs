//! Data editing: editability, `preview_changes` and `apply_changes` for row change sets.
//!
//! Keys: the primary key, else the first unique key whose columns are all `NOT NULL`, else the
//! `ROWID` pseudo column (the grid then shows an extra `ROWID` column, see `data::table_meta`).
//! Values are sent as bind variables; the preview shows the same statements with the values
//! inlined. Updates and deletes must hit exactly one row, otherwise everything is rolled back.

use dbstudio_core::{
    ApplyResult, CellEdit, ChangeSet, EditValue, Editability, Error, NotEditableReason, ObjectKind,
    Result, RowChange, RowChangeSet, Value,
};
use oracle::Connection;
use oracle::sql_type::ToSql;

use crate::catalog::{self, KeyInfo};
use crate::error::map_error;
use crate::sql::{ROWID, hex, qualified, quote_ident, quote_literal};

/// Editability of the table `schema.name`.
pub fn editability(
    conn: &Connection,
    schema: &str,
    name: &str,
    read_only_conn: bool,
) -> Result<Editability> {
    let info = catalog::key_info(conn, schema, name)?;
    Ok(editability_of(&info, read_only_conn))
}

/// Pure part of [`editability`].
pub fn editability_of(info: &KeyInfo, read_only_conn: bool) -> Editability {
    let not_editable = |reason| Editability {
        editable: false,
        key_columns: Vec::new(),
        read_only_columns: info.read_only_columns.clone(),
        reason: Some(reason),
        atomic: true,
        notes: vec![],
    };
    if info.kind != ObjectKind::Table {
        return not_editable(NotEditableReason::NotATable);
    }
    if read_only_conn {
        return not_editable(NotEditableReason::ReadOnlyConnection);
    }
    let key_columns = if !info.primary_key.is_empty() {
        info.primary_key.clone()
    } else if let Some(u) = info.unique_keys.first() {
        u.clone()
    } else {
        vec![ROWID.to_string()]
    };
    Editability {
        editable: true,
        key_columns,
        read_only_columns: info.read_only_columns.clone(),
        reason: None,
        atomic: true,
        notes: vec![],
    }
}

/// Bind variable for `v`.
pub fn bind_of(v: &Value) -> Result<Box<dyn ToSql>> {
    Ok(match v {
        Value::Null => Box::new(Option::<String>::None),
        Value::Bool(b) => Box::new(i64::from(*b)),
        Value::Int(i) => Box::new(*i),
        Value::UInt(u) => Box::new(*u),
        Value::Float(f) => Box::new(*f),
        Value::Decimal(s) | Value::Text(s) | Value::Uuid(s) | Value::Json(s) | Value::Time(s) => {
            Box::new(s.clone())
        }
        Value::Date(s) | Value::DateTime(s) => Box::new(s.clone()),
        Value::Timestamp(s) => Box::new(normalize_offset(s)),
        Value::Bytes(b) => Box::new(b.clone()),
        Value::Ext(e) => Box::new(e.text.clone()),
        Value::Array(_) | Value::Document(_) => {
            return Err(Error::Unsupported("array_and_document_values".into()));
        }
    })
}

/// `...Z` becomes `...+00:00` (the format of `TO_TIMESTAMP_TZ` needs an offset).
fn normalize_offset(s: &str) -> String {
    match s.strip_suffix('Z') {
        Some(rest) => format!("{rest}+00:00"),
        None => s.to_string(),
    }
}

/// SQL expression that turns the bind `:n` into the value (dates need a conversion function).
pub fn placeholder(v: &Value, n: usize) -> String {
    match v {
        Value::Date(_) => format!("TO_DATE(:{n}, 'YYYY-MM-DD')"),
        Value::DateTime(s) => format!(
            "TO_TIMESTAMP(:{n}, 'YYYY-MM-DD\"T\"HH24:MI:SS{}')",
            if s.contains('.') { ".FF" } else { "" }
        ),
        Value::Timestamp(s) => format!(
            "TO_TIMESTAMP_TZ(:{n}, 'YYYY-MM-DD\"T\"HH24:MI:SS{}TZH:TZM')",
            if s.contains('.') { ".FF" } else { "" }
        ),
        _ => format!(":{n}"),
    }
}

/// The same expression with the value inlined (preview only).
fn literal_expr(v: &Value) -> Result<String> {
    Ok(match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => i64::from(*b).to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => {
            if f.is_finite() {
                format!("{f}")
            } else if f.is_nan() {
                "BINARY_DOUBLE_NAN".to_string()
            } else if *f > 0.0 {
                "BINARY_DOUBLE_INFINITY".to_string()
            } else {
                "-BINARY_DOUBLE_INFINITY".to_string()
            }
        }
        Value::Decimal(s) => {
            if s.parse::<f64>().is_ok() && !s.contains(['e', 'E']) {
                s.clone()
            } else {
                format!("TO_NUMBER({})", quote_literal(s)?)
            }
        }
        Value::Text(s) | Value::Uuid(s) | Value::Json(s) | Value::Time(s) => quote_literal(s)?,
        Value::Date(s) => format!("TO_DATE({}, 'YYYY-MM-DD')", quote_literal(s)?),
        Value::DateTime(s) => format!(
            "TO_TIMESTAMP({}, 'YYYY-MM-DD\"T\"HH24:MI:SS{}')",
            quote_literal(s)?,
            if s.contains('.') { ".FF" } else { "" }
        ),
        Value::Timestamp(s) => format!(
            "TO_TIMESTAMP_TZ({}, 'YYYY-MM-DD\"T\"HH24:MI:SS{}TZH:TZM')",
            quote_literal(&normalize_offset(s))?,
            if s.contains('.') { ".FF" } else { "" }
        ),
        Value::Bytes(b) => format!("HEXTORAW('{}')", hex(b)),
        Value::Ext(e) => quote_literal(&e.text)?,
        Value::Array(_) | Value::Document(_) => {
            return Err(Error::Unsupported("array_and_document_values".into()));
        }
    })
}

/// One statement of a change set.
pub struct Built {
    /// SQL with bind variables.
    pub sql: String,
    /// The same statement with the values inlined.
    pub preview: String,
    pub binds: Vec<Box<dyn ToSql>>,
    /// Statement must change exactly one row (updates and deletes by key).
    pub exactly_one: bool,
}

impl std::fmt::Debug for Built {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Built").field("sql", &self.sql).finish()
    }
}

/// Accumulates bound and literal text of one statement.
#[derive(Default)]
struct Parts {
    binds: Vec<Box<dyn ToSql>>,
}

impl Parts {
    /// `(bound expression, literal expression)` of `v`, registering its bind.
    fn value(&mut self, v: &Value) -> Result<(String, String)> {
        if v.is_null() {
            self.binds.push(bind_of(v)?);
            let n = self.binds.len();
            return Ok((format!(":{n}"), "NULL".to_string()));
        }
        let literal = literal_expr(v)?;
        self.binds.push(bind_of(v)?);
        let n = self.binds.len();
        Ok((placeholder(v, n), literal))
    }

    fn text(&mut self, s: &str) -> Result<(String, String)> {
        let literal = quote_literal(s)?;
        self.binds.push(Box::new(s.to_string()));
        Ok((format!(":{}", self.binds.len()), literal))
    }

    fn edit(&mut self, e: &EditValue) -> Result<(String, String)> {
        match e {
            EditValue::Value(v) => self.value(v),
            EditValue::Text(s) => self.text(s),
            EditValue::Default => Ok(("DEFAULT".to_string(), "DEFAULT".to_string())),
        }
    }
}

fn key_condition(parts: &mut Parts, columns: &[String], key: &[Value]) -> Result<(String, String)> {
    if columns.is_empty() {
        return Err(Error::query("the change set has no key columns"));
    }
    if columns.len() != key.len() {
        return Err(Error::query(format!(
            "the key has {} values for {} key columns",
            key.len(),
            columns.len()
        )));
    }
    let mut bound = Vec::new();
    let mut literal = Vec::new();
    for (col, v) in columns.iter().zip(key) {
        if col == ROWID {
            let (b, l) = match v {
                Value::Ext(e) => parts.text(&e.text)?,
                Value::Text(s) => parts.text(s)?,
                _ => return Err(Error::query("a ROWID key must be a rowid or text")),
            };
            bound.push(format!("ROWID = CHARTOROWID({b})"));
            literal.push(format!("ROWID = CHARTOROWID({l})"));
            continue;
        }
        let q = quote_ident(col)?;
        if v.is_null() {
            bound.push(format!("{q} IS NULL"));
            literal.push(format!("{q} IS NULL"));
        } else {
            let (b, l) = parts.value(v)?;
            bound.push(format!("{q} = {b}"));
            literal.push(format!("{q} = {l}"));
        }
    }
    Ok((bound.join(" AND "), literal.join(" AND ")))
}

fn assignments(parts: &mut Parts, set: &[CellEdit]) -> Result<(Vec<String>, Vec<String>)> {
    let mut bound = Vec::new();
    let mut literal = Vec::new();
    for cell in set {
        let q = quote_ident(&cell.column)?;
        let (b, l) = parts.edit(&cell.value)?;
        bound.push(format!("{q} = {b}"));
        literal.push(format!("{q} = {l}"));
    }
    Ok((bound, literal))
}

/// Build the statements of `set` for the table `schema.name`. `first_column` is used for inserts
/// without any value (`VALUES (DEFAULT)`).
pub fn build(
    set: &RowChangeSet,
    schema: &str,
    name: &str,
    first_column: &str,
) -> Result<Vec<Built>> {
    let table = qualified(schema, name)?;
    let mut out = Vec::with_capacity(set.changes.len());
    for change in &set.changes {
        let mut parts = Parts::default();
        let built = match change {
            RowChange::Update { key, set: edits } => {
                if edits.is_empty() {
                    return Err(Error::query("an update needs at least one changed cell"));
                }
                let (sb, sl) = assignments(&mut parts, edits)?;
                let (wb, wl) = key_condition(&mut parts, &set.key_columns, key)?;
                (
                    format!("UPDATE {table} SET {} WHERE {wb}", sb.join(", ")),
                    format!("UPDATE {table} SET {} WHERE {wl}", sl.join(", ")),
                    true,
                )
            }
            RowChange::Insert { values } => {
                if values.is_empty() {
                    let q = quote_ident(first_column)?;
                    let sql = format!("INSERT INTO {table} ({q}) VALUES (DEFAULT)");
                    (sql.clone(), sql, false)
                } else {
                    let mut cols = Vec::new();
                    let mut bound = Vec::new();
                    let mut literal = Vec::new();
                    for cell in values {
                        cols.push(quote_ident(&cell.column)?);
                        let (b, l) = parts.edit(&cell.value)?;
                        bound.push(b);
                        literal.push(l);
                    }
                    (
                        format!(
                            "INSERT INTO {table} ({}) VALUES ({})",
                            cols.join(", "),
                            bound.join(", ")
                        ),
                        format!(
                            "INSERT INTO {table} ({}) VALUES ({})",
                            cols.join(", "),
                            literal.join(", ")
                        ),
                        false,
                    )
                }
            }
            RowChange::Delete { key } => {
                let (wb, wl) = key_condition(&mut parts, &set.key_columns, key)?;
                (
                    format!("DELETE FROM {table} WHERE {wb}"),
                    format!("DELETE FROM {table} WHERE {wl}"),
                    true,
                )
            }
        };
        out.push(Built {
            sql: built.0,
            preview: built.1,
            binds: parts.binds,
            exactly_one: built.2,
        });
    }
    Ok(out)
}

/// Statements `apply_changes` would run.
pub fn preview(changes: &ChangeSet, first_column: &str) -> Result<Vec<String>> {
    let ChangeSet::Rows(set) = changes else {
        return Err(Error::Unsupported("data_editing".into()));
    };
    let (schema, name) = crate::sql::relation_of(&set.table)?;
    Ok(build(set, &schema, &name, first_column)?
        .into_iter()
        .map(|b| b.preview)
        .collect())
}

/// Run the statements in one transaction; any failure rolls everything back.
pub fn apply(conn: &Connection, built: &[Built]) -> Result<ApplyResult> {
    let mut affected = Vec::with_capacity(built.len());
    for (i, b) in built.iter().enumerate() {
        let index = u32::try_from(i).unwrap_or(u32::MAX);
        let fail = |message: String| Error::ChangeFailed {
            index,
            message,
            applied: 0,
        };
        let refs: Vec<&dyn ToSql> = b.binds.iter().map(|x| x.as_ref()).collect();
        let stmt = conn.execute(&b.sql, &refs).map_err(|e| {
            fail(match map_error(e) {
                Error::Query { message, .. } => message,
                other => other.message(),
            })
        })?;
        let rows = stmt.row_count().map_err(|e| fail(map_error(e).message()))?;
        if b.exactly_one && rows != 1 {
            return Err(fail(format!(
                "the statement changed {rows} rows instead of exactly 1 (the row may have been \
                 changed or deleted by someone else)"
            )));
        }
        affected.push(rows);
    }
    conn.commit().map_err(map_error)?;
    Ok(ApplyResult {
        applied: u32::try_from(built.len()).unwrap_or(u32::MAX),
        affected,
        atomic: true,
    })
}

#[cfg(test)]
mod tests {
    use dbstudio_core::{ExtValue, ObjectKind, ObjectPath};

    use super::*;

    fn table() -> ObjectPath {
        ObjectPath::root()
            .child(ObjectKind::Schema, "HR")
            .child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, "EMP")
    }

    fn edit(column: &str, value: EditValue) -> CellEdit {
        CellEdit {
            column: column.into(),
            value,
        }
    }

    fn info(pk: &[&str], unique: &[&[&str]], kind: ObjectKind) -> KeyInfo {
        KeyInfo {
            kind,
            primary_key: pk.iter().map(|s| s.to_string()).collect(),
            unique_keys: unique
                .iter()
                .map(|u| u.iter().map(|s| s.to_string()).collect())
                .collect(),
            read_only_columns: vec!["GEN".into()],
            columns: vec![("ID".into(), false)],
            index_organized: false,
        }
    }

    #[test]
    fn editability_prefers_pk_then_unique_then_rowid() {
        let e = editability_of(&info(&["ID"], &[&["CODE"]], ObjectKind::Table), false);
        assert!(e.editable && e.atomic);
        assert_eq!(e.key_columns, ["ID"]);
        assert_eq!(e.read_only_columns, ["GEN"]);
        let e = editability_of(&info(&[], &[&["CODE"]], ObjectKind::Table), false);
        assert_eq!(e.key_columns, ["CODE"]);
        let e = editability_of(&info(&[], &[], ObjectKind::Table), false);
        assert_eq!(e.key_columns, ["ROWID"]);
        let e = editability_of(&info(&["ID"], &[], ObjectKind::View), false);
        assert!(!e.editable);
        assert_eq!(e.reason, Some(NotEditableReason::NotATable));
        let e = editability_of(&info(&["ID"], &[], ObjectKind::Table), true);
        assert_eq!(e.reason, Some(NotEditableReason::ReadOnlyConnection));
    }

    #[test]
    fn statements_use_binds_and_preview_inlines_values() {
        let set = RowChangeSet {
            table: table(),
            key_columns: vec!["ID".into()],
            changes: vec![
                RowChange::Update {
                    key: vec![Value::Int(7)],
                    set: vec![
                        edit("NAME", EditValue::Text("O'Neil".into())),
                        edit("SALARY", EditValue::Value(Value::Decimal("10.5".into()))),
                        edit("BONUS", EditValue::Default),
                        edit("NOTE", EditValue::Value(Value::Null)),
                    ],
                },
                RowChange::Insert {
                    values: vec![
                        edit("ID", EditValue::Value(Value::Int(8))),
                        edit("HIRED", EditValue::Value(Value::Date("2024-05-31".into()))),
                    ],
                },
                RowChange::Delete {
                    key: vec![Value::Int(9)],
                },
            ],
        };
        let built = build(&set, "HR", "EMP", "ID").unwrap();
        assert_eq!(
            built[0].sql,
            "UPDATE \"HR\".\"EMP\" SET \"NAME\" = :1, \"SALARY\" = :2, \"BONUS\" = DEFAULT, \"NOTE\" = :3 WHERE \"ID\" = :4"
        );
        assert_eq!(built[0].binds.len(), 4);
        assert!(built[0].exactly_one);
        assert_eq!(
            built[0].preview,
            "UPDATE \"HR\".\"EMP\" SET \"NAME\" = 'O''Neil', \"SALARY\" = 10.5, \"BONUS\" = DEFAULT, \"NOTE\" = NULL WHERE \"ID\" = 7"
        );
        assert_eq!(
            built[1].sql,
            "INSERT INTO \"HR\".\"EMP\" (\"ID\", \"HIRED\") VALUES (:1, TO_DATE(:2, 'YYYY-MM-DD'))"
        );
        assert_eq!(
            built[1].preview,
            "INSERT INTO \"HR\".\"EMP\" (\"ID\", \"HIRED\") VALUES (8, TO_DATE('2024-05-31', 'YYYY-MM-DD'))"
        );
        assert!(!built[1].exactly_one);
        assert_eq!(built[2].sql, "DELETE FROM \"HR\".\"EMP\" WHERE \"ID\" = :1");
        let previews = preview(&ChangeSet::Rows(set), "ID").unwrap();
        assert_eq!(previews.len(), 3);
    }

    #[test]
    fn rowid_and_null_keys() {
        let set = RowChangeSet {
            table: table(),
            key_columns: vec!["ROWID".into()],
            changes: vec![RowChange::Delete {
                key: vec![Value::Ext(ExtValue::new("rowid", "AAAB1cAAEAAAAD7AAA"))],
            }],
        };
        let b = build(&set, "HR", "EMP", "ID").unwrap();
        assert_eq!(
            b[0].sql,
            "DELETE FROM \"HR\".\"EMP\" WHERE ROWID = CHARTOROWID(:1)"
        );
        assert_eq!(
            b[0].preview,
            "DELETE FROM \"HR\".\"EMP\" WHERE ROWID = CHARTOROWID('AAAB1cAAEAAAAD7AAA')"
        );
        let set = RowChangeSet {
            table: table(),
            key_columns: vec!["A".into(), "B".into()],
            changes: vec![RowChange::Delete {
                key: vec![Value::Int(1), Value::Null],
            }],
        };
        let b = build(&set, "HR", "EMP", "A").unwrap();
        assert_eq!(
            b[0].sql,
            "DELETE FROM \"HR\".\"EMP\" WHERE \"A\" = :1 AND \"B\" IS NULL"
        );
    }

    #[test]
    fn invalid_change_sets_are_rejected() {
        let mut set = RowChangeSet {
            table: table(),
            key_columns: vec![],
            changes: vec![RowChange::Delete {
                key: vec![Value::Int(1)],
            }],
        };
        assert!(build(&set, "HR", "EMP", "A").is_err(), "no key columns");
        set.key_columns = vec!["ID".into()];
        set.changes = vec![RowChange::Update {
            key: vec![Value::Int(1)],
            set: vec![],
        }];
        assert!(build(&set, "HR", "EMP", "A").is_err(), "empty update");
        set.changes = vec![RowChange::Delete { key: vec![] }];
        assert!(build(&set, "HR", "EMP", "A").is_err(), "short key");
        set.changes = vec![RowChange::Insert {
            values: vec![edit("A", EditValue::Value(Value::Array(vec![])))],
        }];
        assert_eq!(
            build(&set, "HR", "EMP", "A").unwrap_err().kind(),
            "unsupported"
        );
        assert!(
            preview(
                &ChangeSet::Keys(dbstudio_core::KeyChangeSet {
                    database: ObjectPath::root(),
                    changes: vec![]
                }),
                "A"
            )
            .is_err()
        );
    }

    #[test]
    fn empty_insert_uses_default_values() {
        let set = RowChangeSet {
            table: table(),
            key_columns: vec!["ID".into()],
            changes: vec![RowChange::Insert { values: vec![] }],
        };
        let b = build(&set, "HR", "EMP", "ID").unwrap();
        assert_eq!(
            b[0].sql,
            "INSERT INTO \"HR\".\"EMP\" (\"ID\") VALUES (DEFAULT)"
        );
    }

    #[test]
    fn temporal_placeholders_follow_the_text_precision() {
        assert_eq!(
            placeholder(&Value::DateTime("2024-05-31T13:45:00".into()), 2),
            "TO_TIMESTAMP(:2, 'YYYY-MM-DD\"T\"HH24:MI:SS')"
        );
        assert_eq!(
            placeholder(&Value::DateTime("2024-05-31T13:45:00.25".into()), 2),
            "TO_TIMESTAMP(:2, 'YYYY-MM-DD\"T\"HH24:MI:SS.FF')"
        );
        assert_eq!(
            placeholder(&Value::Timestamp("2024-05-31T13:45:00Z".into()), 1),
            "TO_TIMESTAMP_TZ(:1, 'YYYY-MM-DD\"T\"HH24:MI:SSTZH:TZM')"
        );
        assert_eq!(
            normalize_offset("2024-05-31T13:45:00Z"),
            "2024-05-31T13:45:00+00:00"
        );
        assert_eq!(
            literal_expr(&Value::Bytes(vec![1, 255])).unwrap(),
            "HEXTORAW('01FF')"
        );
    }
}
