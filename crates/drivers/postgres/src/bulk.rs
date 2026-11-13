//! PostgreSQL driver: bulk insert for import, called by the lease. Owned by work package G.
//!
//! Plain inserts use `COPY ... FROM STDIN` (text format); conflict handling falls back to
//! `INSERT ... ON CONFLICT`.

use bytes::Bytes;
use dbstudio_core::{
    BulkInsertRequest, BulkInsertResult, EditValue, Error, OnConflict, QueryRequest, Result,
    Session, Value, execute_collect,
};
use futures_util::SinkExt;

use crate::details;
use crate::error::map_error;
use crate::session::PostgresSession;
use crate::sql;

/// PostgreSQL array text form of `items`.
fn array_text(items: &[Value]) -> String {
    let mut s = String::from("{");
    for (i, v) in items.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        match v {
            Value::Null => s.push_str("NULL"),
            Value::Array(inner) => s.push_str(&array_text(inner)),
            other => {
                let text = value_text(other);
                s.push('"');
                for c in text.chars() {
                    if c == '"' || c == '\\' {
                        s.push('\\');
                    }
                    s.push(c);
                }
                s.push('"');
            }
        }
    }
    s.push('}');
    s
}

/// Text of a value as PostgreSQL's input functions read it.
fn value_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => if *b { "t" } else { "f" }.into(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => format!("{f:?}"),
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            format!("\\x{hex}")
        }
        Value::Array(items) => array_text(items),
        Value::Document(_) => "{}".into(),
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::Timestamp(s)
        | Value::Uuid(s)
        | Value::Json(s) => s.clone(),
        Value::Ext(e) => e.text.clone(),
    }
}

/// Append `text` escaped for the COPY text format.
fn copy_escape(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
}

/// `COPY` text of the rows; `None` when a row needs `DEFAULT` (COPY cannot express it).
pub(crate) fn copy_text(rows: &[Vec<EditValue>]) -> Option<String> {
    let mut out = String::new();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.push('\t');
            }
            match cell {
                EditValue::Default => return None,
                EditValue::Value(Value::Null) => out.push_str("\\N"),
                EditValue::Value(v) => copy_escape(&mut out, &value_text(v)),
                EditValue::Text(s) => copy_escape(&mut out, s),
            }
        }
        out.push('\n');
    }
    Some(out)
}

fn literal(v: &EditValue) -> String {
    match v {
        EditValue::Default => "DEFAULT".into(),
        EditValue::Text(s) => sql::quote_literal(s),
        EditValue::Value(Value::Null) => "NULL".into(),
        EditValue::Value(Value::Int(i)) => i.to_string(),
        EditValue::Value(Value::UInt(i)) => i.to_string(),
        EditValue::Value(Value::Bool(b)) => b.to_string(),
        EditValue::Value(Value::Bytes(b)) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            format!("'\\x{hex}'::bytea")
        }
        EditValue::Value(v) => sql::quote_literal(&value_text(v)),
    }
}

/// `INSERT ... [ON CONFLICT ...]` statement of a request.
pub(crate) fn insert_sql(table: &str, req: &BulkInsertRequest, key: &[String]) -> Result<String> {
    let cols = req
        .columns
        .iter()
        .map(|c| sql::quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let rows = req
        .rows
        .iter()
        .map(|r| format!("({})", r.iter().map(literal).collect::<Vec<_>>().join(", ")))
        .collect::<Vec<_>>()
        .join(",\n");
    let mut s = format!("INSERT INTO {table} ({cols}) VALUES\n{rows}");
    let conflict: Vec<String> = if req.conflict_columns.is_empty() {
        key.to_vec()
    } else {
        req.conflict_columns.clone()
    };
    match req.on_conflict {
        OnConflict::Error => {}
        OnConflict::Skip => s.push_str("\nON CONFLICT DO NOTHING"),
        OnConflict::Update => {
            if conflict.is_empty() {
                return Err(Error::Other(
                    "updating on conflict needs a primary key or conflict columns".into(),
                ));
            }
            let sets: Vec<String> = req
                .columns
                .iter()
                .filter(|c| !conflict.contains(c))
                .map(|c| format!("{0} = EXCLUDED.{0}", sql::quote_ident(c)))
                .collect();
            let keys = conflict
                .iter()
                .map(|c| sql::quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ");
            if sets.is_empty() {
                s.push_str(&format!("\nON CONFLICT ({keys}) DO NOTHING"));
            } else {
                s.push_str(&format!(
                    "\nON CONFLICT ({keys}) DO UPDATE SET {}",
                    sets.join(", ")
                ));
            }
        }
    }
    Ok(s)
}

/// Insert rows in bulk.
#[allow(dead_code)]
pub(crate) async fn bulk_insert(
    session: &PostgresSession,
    req: BulkInsertRequest,
) -> Result<BulkInsertResult> {
    let target = details::resolve_relation(&req.table)?;
    let table = sql::qualified(&target.schema, &target.name);
    if req.rows.is_empty() {
        return Ok(BulkInsertResult::default());
    }
    if req.on_conflict == OnConflict::Error {
        if let Some(text) = copy_text(&req.rows) {
            let cols = req
                .columns
                .iter()
                .map(|c| sql::quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ");
            let conn = session.shared.acquire(&target.database).await?;
            let sink = conn
                .client
                .copy_in::<_, Bytes>(&format!("COPY {table} ({cols}) FROM STDIN"))
                .await
                .map_err(map_error)?;
            futures_util::pin_mut!(sink);
            sink.send(Bytes::from(text)).await.map_err(map_error)?;
            let inserted = sink.finish().await.map_err(map_error)?;
            return Ok(BulkInsertResult {
                inserted,
                ..BulkInsertResult::default()
            });
        }
    }
    let key = if req.on_conflict == OnConflict::Update && req.conflict_columns.is_empty() {
        session
            .table_details(&req.table)
            .await?
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect()
    } else {
        Vec::new()
    };
    let mut query = QueryRequest::new(insert_sql(&table, &req, &key)?);
    query.database = Some(target.database);
    let results = execute_collect(session, query).await?;
    let affected = match results.first().map(|r| &r.outcome) {
        Some(dbstudio_core::StatementOutcome::Affected { rows }) => *rows,
        _ => 0,
    };
    let total = req.rows.len() as u64;
    Ok(match req.on_conflict {
        OnConflict::Skip => BulkInsertResult {
            inserted: affected,
            updated: 0,
            skipped: total.saturating_sub(affected),
        },
        _ => BulkInsertResult {
            inserted: affected,
            ..BulkInsertResult::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::ObjectPath;

    fn text(s: &str) -> EditValue {
        EditValue::Text(s.into())
    }

    #[test]
    fn copy_text_escapes_and_marks_nulls() {
        let rows = vec![
            vec![text("a\tb"), EditValue::Value(Value::Null), text("x\\y\nz")],
            vec![
                EditValue::Value(Value::Bytes(vec![1, 255])),
                EditValue::Value(Value::Bool(true)),
                EditValue::Value(Value::Array(vec![Value::Int(1), Value::Null])),
            ],
        ];
        assert_eq!(
            copy_text(&rows).unwrap(),
            "a\\tb\t\\N\tx\\\\y\\nz\n\\\\x01ff\tt\t{\"1\",NULL}\n"
        );
        assert!(copy_text(&[vec![EditValue::Default]]).is_none());
    }

    #[test]
    fn insert_statements() {
        let req = |c| BulkInsertRequest {
            table: ObjectPath::root(),
            columns: vec!["id".into(), "name".into()],
            rows: vec![vec![text("1"), text("it's")]],
            on_conflict: c,
            conflict_columns: vec![],
        };
        let t = "\"s\".\"t\"";
        assert_eq!(
            insert_sql(t, &req(OnConflict::Error), &[]).unwrap(),
            "INSERT INTO \"s\".\"t\" (\"id\", \"name\") VALUES\n('1', 'it''s')"
        );
        assert!(
            insert_sql(t, &req(OnConflict::Skip), &[])
                .unwrap()
                .ends_with("ON CONFLICT DO NOTHING")
        );
        assert!(
            insert_sql(t, &req(OnConflict::Update), &["id".into()])
                .unwrap()
                .ends_with("ON CONFLICT (\"id\") DO UPDATE SET \"name\" = EXCLUDED.\"name\"")
        );
        assert!(insert_sql(t, &req(OnConflict::Update), &[]).is_err());
    }
}
