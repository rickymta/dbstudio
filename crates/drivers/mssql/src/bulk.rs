//! `bulk_insert`: rows written with multi-row `INSERT ... VALUES` statements (up to 1000 rows
//! each), `INSERT ... WHERE NOT EXISTS` for `Skip` and `MERGE` for `Update`.
//!
//! Literals are rendered by the driver so every column type is coerced by the server like a
//! typed-in value; `NULL` is cast to the column type in the derived-table forms, where an
//! untyped `NULL` would become `int`.

use dbstudio_core::{BulkInsertRequest, EditValue, Error, OnConflict, Result};

use crate::sql::{ObjRef, lit, quote, value_lit};

/// Rows per statement (the T-SQL limit of a `VALUES` list is 1000; smaller statements keep the
/// server's compile memory and time low).
pub const ROWS_PER_STATEMENT: usize = 500;

/// What a statement reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reports {
    /// Affected rows are inserted rows; the rest were skipped.
    Inserted { rows: usize },
    /// A result set of `$action` values (`INSERT` / `UPDATE`).
    Actions { rows: usize },
}

fn cell(v: &EditValue) -> String {
    match v {
        EditValue::Value(v) => value_lit(v),
        EditValue::Text(t) => lit(t),
        EditValue::Default => "DEFAULT".to_string(),
    }
}

/// `types[i]` is the DDL type of column `i` (for casting `NULL`).
pub fn statements(
    obj: &ObjRef,
    req: &BulkInsertRequest,
    types: &[String],
    key_columns: &[String],
) -> Result<Vec<(String, Reports)>> {
    if req.columns.is_empty() {
        return Err(Error::Invalid("bulk insert without columns".into()));
    }
    for r in &req.rows {
        if r.len() != req.columns.len() {
            return Err(Error::Invalid(format!(
                "a row has {} values for {} columns",
                r.len(),
                req.columns.len()
            )));
        }
    }
    let table = obj.full();
    let cols: Vec<String> = req.columns.iter().map(|c| quote(c)).collect();
    let conflict: Vec<String> = if req.conflict_columns.is_empty() {
        key_columns.to_vec()
    } else {
        req.conflict_columns.clone()
    };
    if req.on_conflict != OnConflict::Error && conflict.is_empty() {
        return Err(Error::Unsupported(
            "bulk insert with a conflict policy needs a primary key or conflict columns".into(),
        ));
    }
    for k in &conflict {
        if req.on_conflict != OnConflict::Error
            && !req.columns.iter().any(|c| c.eq_ignore_ascii_case(k))
        {
            return Err(Error::Invalid(format!(
                "conflict column `{k}` is not among the inserted columns"
            )));
        }
    }
    // `DEFAULT` is only valid in a plain `VALUES` list, not in a derived table.
    if req.on_conflict != OnConflict::Error
        && req
            .rows
            .iter()
            .any(|r| r.iter().any(|v| matches!(v, EditValue::Default)))
    {
        return Err(Error::Unsupported(
            "DEFAULT cannot be used together with a conflict policy".into(),
        ));
    }
    // In a derived table each literal gets its own type, so values are cast to the column type
    // (strings and binaries are left alone: a cast would silently truncate them).
    let typed = |v: &EditValue, i: usize| {
        let ty = types.get(i).map(String::as_str).unwrap_or("NVARCHAR(MAX)");
        let upper = ty.to_ascii_uppercase();
        let keep = [
            "CHAR",
            "VARCHAR",
            "NCHAR",
            "NVARCHAR",
            "BINARY",
            "VARBINARY",
            "XML",
            "TEXT",
            "NTEXT",
            "IMAGE",
            "ROWVERSION",
            "SQL_VARIANT",
            "[",
        ]
        .iter()
        .any(|p| upper.starts_with(p));
        match v {
            EditValue::Value(dbstudio_core::Value::Null) => {
                let ty = if upper.starts_with("ROWVERSION") {
                    "VARBINARY(8)"
                } else {
                    ty
                };
                format!("CAST(NULL AS {ty})")
            }
            other if keep => cell(other),
            other => format!("CAST({} AS {ty})", cell(other)),
        }
    };
    let mut out = Vec::new();
    for chunk in req.rows.chunks(ROWS_PER_STATEMENT) {
        let n = chunk.len();
        match req.on_conflict {
            OnConflict::Error => {
                let rows: Vec<String> = chunk
                    .iter()
                    .map(|r| format!("({})", r.iter().map(cell).collect::<Vec<_>>().join(", ")))
                    .collect();
                out.push((
                    format!(
                        "INSERT INTO {table} ({}) VALUES\n{};",
                        cols.join(", "),
                        rows.join(",\n")
                    ),
                    Reports::Inserted { rows: n },
                ));
            }
            OnConflict::Skip | OnConflict::Update => {
                let rows: Vec<String> = chunk
                    .iter()
                    .map(|r| {
                        format!(
                            "({})",
                            r.iter()
                                .enumerate()
                                .map(|(i, v)| typed(v, i))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
                    .collect();
                let src_cols = cols.join(", ");
                let on = conflict
                    .iter()
                    .map(|k| format!("t.{} = v.{}", quote(k), quote(k)))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                if req.on_conflict == OnConflict::Skip {
                    out.push((
                        format!(
                            "INSERT INTO {table} ({src_cols}) SELECT {src_cols} FROM (VALUES\n{}\n) AS v ({src_cols}) \
                             WHERE NOT EXISTS (SELECT 1 FROM {table} AS t WHERE {on});",
                            rows.join(",\n")
                        ),
                        Reports::Inserted { rows: n },
                    ));
                } else {
                    let set: Vec<String> = req
                        .columns
                        .iter()
                        .filter(|c| !conflict.iter().any(|k| k.eq_ignore_ascii_case(c)))
                        .map(|c| format!("t.{} = v.{}", quote(c), quote(c)))
                        .collect();
                    let matched = if set.is_empty() {
                        String::new()
                    } else {
                        format!("WHEN MATCHED THEN UPDATE SET {}\n", set.join(", "))
                    };
                    let v_cols: Vec<String> = req
                        .columns
                        .iter()
                        .map(|c| format!("v.{}", quote(c)))
                        .collect();
                    out.push((
                        format!(
                            "MERGE {table} AS t USING (VALUES\n{}\n) AS v ({src_cols}) ON {on}\n{matched}\
                             WHEN NOT MATCHED THEN INSERT ({src_cols}) VALUES ({})\nOUTPUT $action;",
                            rows.join(",\n"),
                            v_cols.join(", ")
                        ),
                        Reports::Actions { rows: n },
                    ));
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{ObjectKind, Value};

    fn obj() -> ObjRef {
        ObjRef {
            db: Some("d".into()),
            schema: "s".into(),
            name: "t".into(),
            kind: ObjectKind::Table,
            child: None,
        }
    }

    fn req(on_conflict: OnConflict, rows: usize) -> BulkInsertRequest {
        BulkInsertRequest {
            table: dbstudio_core::ObjectPath::root(),
            columns: vec!["id".into(), "name".into()],
            rows: (0..rows)
                .map(|i| {
                    vec![
                        EditValue::Value(Value::Int(i as i64)),
                        if i == 0 {
                            EditValue::Value(Value::Null)
                        } else {
                            EditValue::Text("x'y".into())
                        },
                    ]
                })
                .collect(),
            on_conflict,
            conflict_columns: vec![],
        }
    }

    #[test]
    fn plain_insert_chunks_rows() {
        let s = statements(&obj(), &req(OnConflict::Error, 2500), &[], &[]).unwrap();
        assert_eq!(s.len(), 5);
        assert_eq!(s[0].1, Reports::Inserted { rows: 500 });
        assert_eq!(s[4].1, Reports::Inserted { rows: 500 });
        assert!(s[0].0.starts_with(
            "INSERT INTO [d].[s].[t] ([id], [name]) VALUES\n(0, NULL),\n(1, N'x''y')"
        ));
    }

    #[test]
    fn skip_and_merge_forms() {
        let types = vec!["INT".to_string(), "NVARCHAR(20)".to_string()];
        let s = statements(&obj(), &req(OnConflict::Skip, 2), &types, &["id".into()]).unwrap();
        assert!(
            s[0].0
                .contains("(CAST(0 AS INT), CAST(NULL AS NVARCHAR(20)))")
        );
        assert!(s[0].0.contains("(CAST(1 AS INT), N'x''y')"));
        assert!(
            s[0].0.contains(
                "WHERE NOT EXISTS (SELECT 1 FROM [d].[s].[t] AS t WHERE t.[id] = v.[id])"
            )
        );
        let s = statements(&obj(), &req(OnConflict::Update, 2), &types, &["id".into()]).unwrap();
        assert!(s[0].0.starts_with("MERGE [d].[s].[t] AS t USING (VALUES"));
        assert!(
            s[0].0
                .contains("WHEN MATCHED THEN UPDATE SET t.[name] = v.[name]")
        );
        assert!(
            s[0].0
                .contains("WHEN NOT MATCHED THEN INSERT ([id], [name]) VALUES (v.[id], v.[name])")
        );
        assert!(s[0].0.ends_with("OUTPUT $action;"));
        assert_eq!(s[0].1, Reports::Actions { rows: 2 });
    }

    #[test]
    fn validation() {
        assert!(statements(&obj(), &req(OnConflict::Skip, 1), &[], &[]).is_err());
        let mut r = req(OnConflict::Error, 1);
        r.rows[0].pop();
        assert!(statements(&obj(), &r, &[], &[]).is_err());
        let mut r = req(OnConflict::Skip, 1);
        r.conflict_columns = vec!["zzz".into()];
        assert!(statements(&obj(), &r, &[], &[]).is_err());
        let mut r = req(OnConflict::Skip, 1);
        r.rows[0][1] = EditValue::Default;
        assert!(
            statements(
                &obj(),
                &r,
                &["INT".into(), "NVARCHAR(5)".into()],
                &["id".into()]
            )
            .is_err()
        );
        let mut r = req(OnConflict::Update, 1);
        r.columns = vec![];
        assert!(statements(&obj(), &r, &[], &[]).is_err());
    }
}
