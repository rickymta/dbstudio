//! Whole-table streaming (export) and bulk insert through the HTTP interface.
//!
//! Rows are encoded as `RowBinaryWithDefaults` (a one-byte "use the default" flag per cell, so
//! `EditValue::Default` works) with the column types read from `system.columns`, and sent in
//! batches of [`BATCH_ROWS`] rows, one `INSERT` per batch (not atomic).

use bytes::Bytes;
use dbstudio_core::{
    BulkInsertRequest, BulkInsertResult, EditValue, Error, OnConflict, Result, TableStreamRequest,
    Value,
};

use crate::browse;
use crate::conn::{Core, Req, quote_ident};
use crate::rowbinary::encode_value;
use crate::sql::{order_clause, quote_table, table_target, where_clause};
use crate::types::CType;

/// Rows per `INSERT` request.
pub(crate) const BATCH_ROWS: usize = 100_000;
/// Soft cap of the encoded size of one request.
const BATCH_BYTES: usize = 48 * 1024 * 1024;

/// The `SELECT` that reads a whole table (export / backup).
pub(crate) async fn table_select(core: &Core, req: &TableStreamRequest) -> Result<String> {
    let (db, table) = table_target(&req.table)?;
    let columns = if req.columns.is_empty() {
        "*".to_string()
    } else {
        req.columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let sorting = if req.order_by_key {
        browse::table_facts(core, &db, &table).await?.sorting_key
    } else {
        String::new()
    };
    Ok(format!(
        "SELECT {columns} FROM {}{}{}",
        quote_table(&db, &table),
        where_clause(&req.filters, req.raw_where.as_deref())?,
        order_clause(&[], &sorting)
    ))
}

fn edit_value(v: &EditValue) -> Option<Value> {
    match v {
        EditValue::Default => None,
        EditValue::Text(s) => Some(Value::Text(s.clone())),
        EditValue::Value(v) => Some(v.clone()),
    }
}

pub(crate) async fn bulk_insert(
    core: &Core,
    session_id: Option<&str>,
    req: BulkInsertRequest,
) -> Result<BulkInsertResult> {
    match req.on_conflict {
        OnConflict::Error => {}
        OnConflict::Skip | OnConflict::Update => {
            return Err(Error::Unsupported(
                "ClickHouse has no unique constraints: only OnConflict::Error (plain insert) is supported"
                    .into(),
            ));
        }
    }
    let (db, table) = table_target(&req.table)?;
    if core.config_read_only {
        return Err(Error::ReadOnlyViolation {
            message: "the connection is read-only".into(),
            position: None,
        });
    }
    if req.columns.is_empty() {
        return Err(Error::Invalid("bulk insert needs at least one column".into()));
    }
    let facts = browse::column_list(core, &db, &table).await?;
    if facts.is_empty() {
        return Err(Error::NotFound(format!("table `{db}`.`{table}`")));
    }
    let mut types: Vec<CType> = Vec::with_capacity(req.columns.len());
    for name in &req.columns {
        let f = facts
            .iter()
            .find(|c| &c.name == name)
            .ok_or_else(|| Error::Invalid(format!("unknown column `{name}`")))?;
        if matches!(f.default_kind.as_str(), "MATERIALIZED" | "ALIAS") {
            return Err(Error::Invalid(format!("column `{name}` is generated (read-only)")));
        }
        types.push(CType::parse_lenient(&f.ty));
    }
    let sql = format!(
        "INSERT INTO {} ({}) FORMAT RowBinaryWithDefaults",
        quote_table(&db, &table),
        req.columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let client_req = Req {
        session_id: session_id.map(str::to_string),
        ..Req::default()
    };
    let mut inserted = 0u64;
    let mut buf: Vec<u8> = Vec::new();
    let mut in_batch = 0u64;
    let flush = |buf: &mut Vec<u8>, n: u64| {
        let data = Bytes::from(std::mem::take(buf));
        let client = core.client(&client_req);
        let sql = sql.clone();
        async move {
            let mut insert = client.insert_formatted_with(sql);
            insert.send(data).await.map_err(crate::error::map_client_error)?;
            insert.end().await.map_err(crate::error::map_client_error)?;
            Ok::<u64, Error>(n)
        }
    };
    for (r, row) in req.rows.iter().enumerate() {
        if row.len() != req.columns.len() {
            return Err(Error::Invalid(format!(
                "row {} has {} values for {} columns",
                r + 1,
                row.len(),
                req.columns.len()
            )));
        }
        for (cell, t) in row.iter().zip(&types) {
            match edit_value(cell) {
                None => buf.push(1),
                Some(v) => {
                    buf.push(0);
                    encode_value(&mut buf, t, &v)
                        .map_err(|e| Error::Invalid(format!("row {}: {}", r + 1, e.message())))?;
                }
            }
        }
        in_batch += 1;
        if in_batch as usize >= BATCH_ROWS || buf.len() >= BATCH_BYTES {
            inserted += flush(&mut buf, in_batch).await?;
            in_batch = 0;
        }
    }
    if in_batch > 0 {
        inserted += flush(&mut buf, in_batch).await?;
    }
    Ok(BulkInsertResult {
        inserted,
        updated: 0,
        skipped: 0,
    })
}
