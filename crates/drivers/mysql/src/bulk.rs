//! MySQL/MariaDB driver: bulk insert for import, called by the lease. Owned by work package G.
//!
//! Multi-row `INSERT ... [ON DUPLICATE KEY UPDATE]` statements.

use dbstudio_core::{
    BulkInsertRequest, BulkInsertResult, EditValue, Error, OnConflict, Result, ScriptDialect,
    Session, Value,
};
use mysql_async::Conn;
use mysql_async::prelude::Queryable;

use crate::error::map_query_error;
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
    let mut s = format!(
        "INSERT INTO {} ({cols}) VALUES\n{values}",
        quote_table(db, table)
    );
    if req.on_conflict == OnConflict::Skip {
        // Not `INSERT IGNORE`: that would also turn bad values into silent warnings. A
        // duplicate is skipped by "updating" a column to itself.
        if let Some(first) = req.columns.first() {
            s.push_str(&format!(
                "\nON DUPLICATE KEY UPDATE {0} = {0}",
                quote_ident(first)
            ));
        }
    }
    if req.on_conflict == OnConflict::Update {
        let sets: Vec<String> = req
            .columns
            .iter()
            .filter(|c| !req.conflict_columns.contains(c))
            .map(|c| format!("{0} = VALUES({0})", quote_ident(c)))
            .collect();
        if sets.is_empty() {
            if let Some(first) = req.columns.first() {
                s.push_str(&format!(
                    "\nON DUPLICATE KEY UPDATE {0} = {0}",
                    quote_ident(first)
                ));
            }
        } else {
            s.push_str(&format!("\nON DUPLICATE KEY UPDATE {}", sets.join(", ")));
        }
    }
    s
}

/// Primary key columns of `db.table`, read on `conn`.
pub(crate) async fn primary_key(conn: &mut Conn, db: &str, table: &str) -> Result<Vec<String>> {
    conn.exec::<String, _, _>(
        "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND CONSTRAINT_NAME = 'PRIMARY' \
         ORDER BY ORDINAL_POSITION",
        (db, table),
    )
    .await
    .map_err(map_query_error)
}

/// `(inserted, updated)` of an `ON DUPLICATE KEY UPDATE` statement over `rows` rows: MySQL
/// reports 1 per inserted and 2 per changed row.
pub(crate) fn split_upsert_count(affected: u64, rows: u64) -> (u64, u64) {
    if affected > rows {
        let updated = affected - rows;
        (rows.saturating_sub(updated), updated)
    } else {
        (affected, 0)
    }
}

/// Insert rows on `conn` (inside its current transaction; nothing is committed here).
pub(crate) async fn bulk_on_conn(
    conn: &mut Conn,
    no_backslash_escapes: bool,
    req: &BulkInsertRequest,
) -> Result<BulkInsertResult> {
    let (db, table) = sql::table_target(&req.table)?;
    let mut req = req.clone();
    if req.on_conflict == OnConflict::Update && req.conflict_columns.is_empty() {
        req.conflict_columns = primary_key(conn, &db, &table).await?;
    }
    let mut result = BulkInsertResult::default();
    for chunk in req.rows.chunks(STATEMENT_ROWS) {
        let text = insert_sql(&db, &table, &req, chunk, no_backslash_escapes);
        conn.query_drop(text).await.map_err(map_query_error)?;
        let affected = conn.affected_rows();
        let n = chunk.len() as u64;
        match req.on_conflict {
            OnConflict::Error => result.inserted += affected.min(n),
            OnConflict::Skip => {
                result.inserted += affected.min(n);
                result.skipped += n.saturating_sub(affected);
            }
            OnConflict::Update => {
                let (i, u) = split_upsert_count(affected, n);
                result.inserted += i;
                result.updated += u;
            }
        }
    }
    Ok(result)
}

/// Insert rows in bulk on a pooled connection of the session.
#[allow(dead_code)]
pub(crate) async fn bulk_insert(
    session: &MySqlSession,
    req: BulkInsertRequest,
) -> Result<BulkInsertResult> {
    let nb = matches!(
        session.script_dialect(),
        ScriptDialect::MySql {
            backslash_escapes: false,
            ..
        }
    );
    let mut conn = session.inner.acquire().await?;
    let r = bulk_on_conn(&mut conn, nb, &req).await;
    let broken = matches!(r, Err(Error::Connection(_) | Error::Timeout));
    session.inner.release(conn, broken).await;
    r
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
        assert!(
            insert_sql("d", "t", &r, &r.rows, true)
                .ends_with("ON DUPLICATE KEY UPDATE `id` = `id`")
        );
        assert_eq!(split_upsert_count(3, 3), (3, 0));
        assert_eq!(split_upsert_count(5, 3), (1, 2));
        assert_eq!(split_upsert_count(0, 3), (0, 0));
        let r = req(OnConflict::Update);
        assert!(
            insert_sql("d", "t", &r, &r.rows, false)
                .ends_with("ON DUPLICATE KEY UPDATE `name` = VALUES(`name`)")
        );
    }
}
