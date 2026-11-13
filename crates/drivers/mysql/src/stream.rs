//! MySQL/MariaDB driver: whole-table streaming for export/backup, called by the lease. Owned by work package G.
//!
//! The rows come through the normal statement path (`query_iter`, one batch at a time), so
//! memory does not grow with the table.

use dbstudio_core::{
    ExecSummary, QueryRequest, Result, ResultSink, ScriptDialect, Session, TableStreamRequest,
};

use crate::session::MySqlSession;
use crate::sql::{self, order_clause, quote_ident, quote_table, where_clause};

/// `SELECT cols FROM db.table [WHERE ...] [ORDER BY key]` of a stream request.
pub(crate) fn select_for(
    req: &TableStreamRequest,
    db: &str,
    table: &str,
    key: &[String],
    no_backslash_escapes: bool,
) -> Result<String> {
    let columns = if req.columns.is_empty() {
        "*".to_string()
    } else {
        req.columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ")
    };
    Ok(format!(
        "SELECT {columns} FROM {}{}{}",
        quote_table(db, table),
        where_clause(&req.filters, req.raw_where.as_deref(), no_backslash_escapes)?,
        order_clause(&[], key)
    ))
}

/// Stream a whole table into `sink`.
#[allow(dead_code)]
pub(crate) async fn stream_table(
    session: &MySqlSession,
    req: TableStreamRequest,
    sink: &mut dyn ResultSink,
) -> Result<ExecSummary> {
    let (db, table) = sql::table_target(&req.table)?;
    let nb = matches!(
        session.script_dialect(),
        ScriptDialect::MySql {
            backslash_escapes: false,
            ..
        }
    );
    let key: Vec<String> = if req.order_by_key {
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
    let mut query = QueryRequest::new(select_for(&req, &db, &table, &key, nb)?);
    query.query_id = req.query_id;
    query.batch_size = req.batch_size;
    query.database = Some(db);
    session.execute(query, sink).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{ColumnFilter, FilterOp, ObjectPath};
    use uuid::Uuid;

    #[test]
    fn builds_the_select() {
        let mut r = TableStreamRequest {
            query_id: Uuid::nil(),
            table: ObjectPath::root(),
            columns: vec![],
            filters: vec![],
            raw_where: None,
            order_by_key: false,
            batch_size: None,
        };
        assert_eq!(
            select_for(&r, "d", "t", &[], false).unwrap(),
            "SELECT * FROM `d`.`t`"
        );
        r.columns = vec!["id".into(), "we`ird".into()];
        r.filters = vec![ColumnFilter {
            column: "id".into(),
            op: FilterOp::Gt,
            values: vec!["5".into()],
        }];
        let sql = select_for(&r, "d", "t", &["id".into()], false).unwrap();
        assert!(
            sql.starts_with("SELECT `id`, `we``ird` FROM `d`.`t` WHERE"),
            "{sql}"
        );
        assert!(sql.ends_with("ORDER BY `id` ASC"), "{sql}");
    }
}
