//! PostgreSQL driver: whole-table streaming for export/backup, called by the lease. Owned by work package G.
//!
//! The rows come through the normal statement path, which reads them in batches from a
//! server-side cursor, so memory does not grow with the table.

use dbstudio_core::{ExecSummary, QueryRequest, Result, ResultSink, Session, TableStreamRequest};

use crate::details;
use crate::session::PostgresSession;
use crate::sql;

/// `SELECT cols FROM table [WHERE ...] [ORDER BY key]` of a stream request.
pub(crate) fn select_for(req: &TableStreamRequest, table: &str, key: &[String]) -> Result<String> {
    let columns = if req.columns.is_empty() {
        "*".to_string()
    } else {
        req.columns
            .iter()
            .map(|c| {
                sql::check_no_nul(c, "column name")?;
                Ok(sql::quote_ident(c))
            })
            .collect::<Result<Vec<_>>>()?
            .join(", ")
    };
    Ok(format!(
        "SELECT {columns} FROM {table}{}{}",
        sql::where_clause(&req.filters, req.raw_where.as_deref())?,
        sql::order_clause(&[], key)?
    ))
}

/// Stream a whole table into `sink`.
#[allow(dead_code)]
pub(crate) async fn stream_table(
    session: &PostgresSession,
    req: TableStreamRequest,
    sink: &mut dyn ResultSink,
) -> Result<ExecSummary> {
    let target = details::resolve_relation(&req.table)?;
    let table = sql::qualified(&target.schema, &target.name);
    let key = if req.order_by_key {
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
    let mut query = QueryRequest::new(select_for(&req, &table, &key)?);
    query.query_id = req.query_id;
    query.batch_size = req.batch_size;
    query.database = Some(target.database);
    session.execute(query, sink).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{ColumnFilter, FilterOp};
    use uuid::Uuid;

    fn req() -> TableStreamRequest {
        TableStreamRequest {
            query_id: Uuid::nil(),
            table: dbstudio_core::ObjectPath::root(),
            columns: vec![],
            filters: vec![],
            raw_where: None,
            order_by_key: false,
            batch_size: None,
        }
    }

    #[test]
    fn builds_the_select() {
        let t = "\"public\".\"t\"";
        assert_eq!(
            select_for(&req(), t, &[]).unwrap(),
            "SELECT * FROM \"public\".\"t\""
        );
        let mut r = req();
        r.columns = vec!["id".into(), "we\"ird".into()];
        r.filters = vec![ColumnFilter {
            column: "id".into(),
            op: FilterOp::Gt,
            values: vec!["5".into()],
        }];
        r.raw_where = Some("n < 10".into());
        let sql = select_for(&r, t, &["id".into()]).unwrap();
        assert!(
            sql.starts_with("SELECT \"id\", \"we\"\"ird\" FROM \"public\".\"t\""),
            "{sql}"
        );
        assert!(sql.contains("WHERE"), "{sql}");
        assert!(sql.contains("n < 10"), "{sql}");
        assert!(sql.ends_with("ORDER BY \"id\" ASC"), "{sql}");
    }
}
