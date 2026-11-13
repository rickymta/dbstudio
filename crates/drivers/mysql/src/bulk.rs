//! MySQL/MariaDB driver: bulk insert for import, called by the lease. Owned by work package G.
//!
//! Multi-row `INSERT [IGNORE] ... [ON DUPLICATE KEY UPDATE]` statements.

use dbstudio_core::{
    BulkInsertRequest, BulkInsertResult, EditValue, OnConflict, QueryRequest, Result,
    ScriptDialect, Session, StatementOutcome, Value, execute_collect,
};

use crate::session::MySqlSession;
use crate::sql::{self, quote_ident, quote_literal, quote_table};

/// Rows per statement (keeps statements well below `max_allowed_packet`).
const STATEMENT_ROWS: usize = 500;

fn literal(v: &EditValue, nb: bool) -> String {
    match v {
        EditValue::Default => "DEFAULT".into(),
        EditValue::Text(s) => quote_literal(s, nb),
        EditValue::Value(v) => match v {
            Value::Null => "NULL".into(),
            Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) if f.is_finite() => format!("{f:?}"),
            Value::Decimal(s) if s.parse::<f64>().is_ok_and(f64::is_finite) => s.clone(),
            Value::Bytes(b) => {
                let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
                format!("X'{hex}'")
            }
            Value::Ext(e) => quote_literal(&e.text, nb),
            Value::Float(f) => quote_literal(&f.to_string(), nb),
            Value::Decimal(s)
            | Value::Text(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::DateTime(s)
            | Value::Timestamp(s)
            | Value::Uuid(s)
            | Value::Json(s) => quote_literal(s, nb),
            Value::Array(_) | Value::Document(_) => "NULL".into(),
        },
    }
}

/// One `INSERT` of `rows`.
pub(crate) fn insert_sql(
    db: &str,
    table: &str,
    req: &BulkInsertRequest,
    rows: &[Vec<EditValue>],
    nb: bool,
) -> String {
    let cols = req
        .columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let values = rows
        .iter()
        .map(|r| {
            format!(
                "({})",
                r.iter()
                    .map(|v| literal(v, nb))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    let verb = if req.on_conflict == OnConflict::Skip {
        "INSERT IGNORE INTO"
    } else {
        "INSERT INTO"
    };
    let mut s = format!(
        "{verb} {} ({cols}) VALUES\n{values}",
        quote_table(db, table)
    );
    if req.on_conflict == OnConflict::Update {
        let sets: Vec<String> = req
            .columns
            .iter()
            .filter(|c| !req.conflict_columns.contains(c))
            .map(|c| format!("{0} = VALUES({0})", quote_ident(c)))
            .collect();
        if sets.is_empty() {
            s = s.replacen("INSERT INTO", "INSERT IGNORE INTO", 1);
        } else {
            s.push_str(&format!("\nON DUPLICATE KEY UPDATE {}", sets.join(", ")));
        }
    }
    s
}

/// Insert rows in bulk.
#[allow(dead_code)]
pub(crate) async fn bulk_insert(
    session: &MySqlSession,
    req: BulkInsertRequest,
) -> Result<BulkInsertResult> {
    let (db, table) = sql::table_target(&req.table)?;
    let nb = matches!(
        session.script_dialect(),
        ScriptDialect::MySql {
            backslash_escapes: false,
            ..
        }
    );
    let mut req = req;
    if req.on_conflict == OnConflict::Update && req.conflict_columns.is_empty() {
        req.conflict_columns = session
            .table_details(&req.table)
            .await?
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect();
    }
    let mut result = BulkInsertResult::default();
    for chunk in req.rows.chunks(STATEMENT_ROWS) {
        let mut query = QueryRequest::new(insert_sql(&db, &table, &req, chunk, nb));
        query.database = Some(db.clone());
        let results = execute_collect(session, query).await?;
        let affected = match results.first().map(|r| &r.outcome) {
            Some(StatementOutcome::Affected { rows }) => *rows,
            _ => 0,
        };
        let n = chunk.len() as u64;
        if req.on_conflict == OnConflict::Skip {
            result.inserted += affected.min(n);
            result.skipped += n.saturating_sub(affected);
        } else {
            result.inserted += affected.min(n);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::ObjectPath;

    fn req(c: OnConflict) -> BulkInsertRequest {
        BulkInsertRequest {
            table: ObjectPath::root(),
            columns: vec!["id".into(), "name".into()],
            rows: vec![vec![
                EditValue::Value(Value::Int(1)),
                EditValue::Text("it's \\".into()),
            ]],
            on_conflict: c,
            conflict_columns: vec!["id".into()],
        }
    }

    #[test]
    fn insert_statements_follow_the_conflict_mode() {
        let r = req(OnConflict::Error);
        assert_eq!(
            insert_sql("d", "t", &r, &r.rows, false),
            "INSERT INTO `d`.`t` (`id`, `name`) VALUES\n(1, 'it''s \\\\')"
        );
        let r = req(OnConflict::Skip);
        assert!(insert_sql("d", "t", &r, &r.rows, true).starts_with("INSERT IGNORE INTO"));
        let r = req(OnConflict::Update);
        assert!(
            insert_sql("d", "t", &r, &r.rows, false)
                .ends_with("ON DUPLICATE KEY UPDATE `name` = VALUES(`name`)")
        );
    }
}
