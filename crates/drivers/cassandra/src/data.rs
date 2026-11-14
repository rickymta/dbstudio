//! Table reads (grid pages, export) and bulk inserts.

use std::time::Instant;

use dbstudio_core::{
    BulkInsertRequest, BulkInsertResult, ColumnFilter, ColumnMeta, EditValue, EndOutcome, Error,
    ExecSummary, FilterOp, OnConflict, Result, ResultSink, StatementEnd, TableDataRequest,
    TableStreamRequest,
};
use futures::StreamExt;
use scylla::response::PagingState;
use scylla::statement::{Consistency, SerialConsistency, Statement};
use scylla::value::Row as ScyllaRow;

use crate::convert::{literal, literal_from_text};
use crate::core::{Core, DEFAULT_BATCH, ExecState};
use crate::error;
use crate::ident::{qualified, quote_ident, quote_text};
use crate::schema::TableInfo;
use crate::tree::{Node, parse_path};
use crate::types::CqlType;

/// Most rows read and discarded to reach a requested offset.
const MAX_SKIP: u64 = 100_000;
const BULK_CONCURRENCY: usize = 64;

/// A `WHERE` clause built from filters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Where {
    /// ` WHERE ...` or empty.
    pub clause: String,
    /// A filtered column is not part of the primary key (needs `ALLOW FILTERING`).
    pub needs_filtering: bool,
    /// The filters can never match (`IN ()`).
    pub matches_nothing: bool,
    /// The user typed a raw `WHERE` body.
    pub raw: bool,
}

fn column<'a>(t: &'a TableInfo, name: &str) -> Result<&'a crate::schema::ColumnInfo> {
    t.column(name)
        .ok_or_else(|| Error::Invalid(format!("unknown column `{name}` in {}", t.name)))
}

fn one_value(f: &ColumnFilter) -> Result<&str> {
    f.values
        .first()
        .map(String::as_str)
        .ok_or_else(|| Error::Invalid(format!("filter on `{}` needs a value", f.column)))
}

fn like(f: &ColumnFilter, ty: &CqlType, prefix: &str, suffix: &str) -> Result<String> {
    let v = one_value(f)?;
    if !matches!(ty.unfrozen(), CqlType::Text | CqlType::Ascii) {
        return Err(Error::Unsupported(format!(
            "text search on the {ty} column `{}` (CQL LIKE works on text columns with a SAI/SASI index)",
            f.column
        )));
    }
    if v.contains('%') {
        return Err(Error::Invalid(
            "CQL LIKE cannot match a literal `%`; remove it from the filter".into(),
        ));
    }
    Ok(format!(
        "{} LIKE {}",
        quote_ident(&f.column),
        quote_text(&format!("{prefix}{v}{suffix}"))
    ))
}

/// Build the `WHERE` of a grid request. See [`FilterOp`] for the contract; CQL cannot express
/// `<>`, `NOT LIKE` and null tests, those are `Unsupported`.
pub(crate) fn build_where(
    t: &TableInfo,
    filters: &[ColumnFilter],
    raw: Option<&str>,
) -> Result<Where> {
    let mut parts: Vec<String> = Vec::new();
    let mut needs_filtering = false;
    for f in filters {
        let col = column(t, &f.column)?;
        if !col.is_key() {
            needs_filtering = true;
        }
        let name = quote_ident(&f.column);
        let part = match f.op {
            FilterOp::Eq => format!("{name} = {}", literal_from_text(one_value(f)?, &col.ty)?),
            FilterOp::Lt => format!("{name} < {}", literal_from_text(one_value(f)?, &col.ty)?),
            FilterOp::Le => format!("{name} <= {}", literal_from_text(one_value(f)?, &col.ty)?),
            FilterOp::Gt => format!("{name} > {}", literal_from_text(one_value(f)?, &col.ty)?),
            FilterOp::Ge => format!("{name} >= {}", literal_from_text(one_value(f)?, &col.ty)?),
            FilterOp::In => {
                if f.values.is_empty() {
                    return Ok(Where {
                        matches_nothing: true,
                        ..Where::default()
                    });
                }
                let lits = f
                    .values
                    .iter()
                    .map(|v| literal_from_text(v, &col.ty))
                    .collect::<Result<Vec<_>>>()?;
                format!("{name} IN ({})", lits.join(", "))
            }
            FilterOp::Contains => match col.ty.unfrozen() {
                CqlType::List(e) | CqlType::Set(e) => {
                    format!("{name} CONTAINS {}", literal_from_text(one_value(f)?, e)?)
                }
                CqlType::Map(_, v) => {
                    format!("{name} CONTAINS {}", literal_from_text(one_value(f)?, v)?)
                }
                _ => like(f, &col.ty, "%", "%")?,
            },
            FilterOp::StartsWith => like(f, &col.ty, "", "%")?,
            FilterOp::EndsWith => like(f, &col.ty, "%", "")?,
            FilterOp::Ne | FilterOp::NotContains | FilterOp::IsNull | FilterOp::IsNotNull => {
                return Err(Error::Unsupported(format!(
                    "filter operator {:?} on `{}`: CQL has no equivalent",
                    f.op, f.column
                )));
            }
        };
        parts.push(part);
    }
    let raw = raw.map(str::trim).filter(|r| !r.is_empty());
    if let Some(r) = raw {
        parts.push(r.trim_end_matches(';').to_string());
    }
    Ok(Where {
        clause: if parts.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", parts.join(" AND "))
        },
        needs_filtering,
        matches_nothing: false,
        raw: raw.is_some(),
    })
}

/// `SELECT cols FROM ks.t WHERE ... [ALLOW FILTERING]`.
pub(crate) fn select_text(
    ks: &str,
    t: &TableInfo,
    columns: &[String],
    w: &Where,
    allow_filtering: bool,
) -> Result<String> {
    let cols = if columns.is_empty() {
        "*".to_string()
    } else {
        columns
            .iter()
            .map(|c| column(t, c).map(|_| quote_ident(c)))
            .collect::<Result<Vec<_>>>()?
            .join(", ")
    };
    Ok(format!(
        "SELECT {cols} FROM {}{}{}",
        qualified(ks, &t.name),
        w.clause,
        if allow_filtering {
            " ALLOW FILTERING"
        } else {
            ""
        }
    ))
}

fn relation_of(node: &Node) -> Result<(String, String)> {
    node.relation()
        .map(|(k, t, _)| (k.to_string(), t.to_string()))
        .ok_or_else(|| Error::Invalid("the path is not a table or view".into()))
}

/// Whether the LWT result says the condition held (`[applied]`).
pub(crate) fn applied(result: scylla::response::query_result::QueryResult) -> Option<bool> {
    if !result.is_rows() {
        return None;
    }
    let rows = result.into_rows_result().ok()?;
    let first = rows.rows::<ScyllaRow>().ok()?.next()?.ok()?;
    first.columns.first()?.as_ref()?.as_boolean()
}

impl Core {
    /// One page of table rows for the data grid.
    pub(crate) async fn fetch(
        &self,
        req: TableDataRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let started = Instant::now();
        if !req.sort.is_empty() {
            return Err(Error::Unsupported("table_sort".into()));
        }
        let snap = self.snapshot().await?;
        let (ks, name) = relation_of(&parse_path(&req.table)?)?;
        let t = snap.relation(&ks, &name)?;
        let w = build_where(t, &req.filters, req.raw_where.as_deref())?;
        if w.matches_nothing {
            sink.on_columns(columns_of(t))?;
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Rows {
                    row_count: 0,
                    truncated: false,
                },
                elapsed_ms: started.elapsed().as_millis() as u64,
            })?;
            return Ok(ExecSummary {
                statements: 1,
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
        }
        let guard = self.register(req.query_id);
        let batch = req
            .batch_size
            .map(|b| b.max(1) as usize)
            .unwrap_or(DEFAULT_BATCH);
        let query_key = format!("{ks}.{name}|{}", w.clause);
        let session = self.pool.base();
        let state = self.initial_state(None);
        let mut allow = w.needs_filtering;
        if allow {
            sink.on_message(
                "warning: this filter is not on the primary key; the query scans the table (ALLOW FILTERING)"
                    .into(),
            )?;
        }
        loop {
            let text = select_text(&ks, t, &[], &w, allow)?;
            let attempt = self.fetch_attempt(
                &session, &state, &text, &query_key, req.offset, req.limit, batch, sink, started,
            );
            let result = tokio::select! {
                r = attempt => r,
                _ = guard.token.cancelled() => Err(Error::Cancelled),
            };
            match result {
                Err(Error::Query { message, .. })
                    if !allow && message.contains("ALLOW FILTERING") =>
                {
                    allow = true;
                    sink.on_message(
                        "warning: the query needs ALLOW FILTERING and scans the table".into(),
                    )?;
                }
                Err(e) => return Err(e),
                Ok(()) => break,
            }
        }
        Ok(ExecSummary {
            statements: 1,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn fetch_attempt(
        &self,
        session: &std::sync::Arc<scylla::client::session::Session>,
        state: &ExecState,
        text: &str,
        query_key: &str,
        offset: u64,
        limit: u64,
        batch: usize,
        sink: &mut dyn ResultSink,
        started: Instant,
    ) -> Result<()> {
        let mut stmt = Statement::new(text.to_string());
        stmt.set_consistency(state.consistency);
        // Where to start: an exact cached state, or the closest one and skip the rest.
        let (mut at, mut paging) = (0u64, PagingState::start());
        if offset > 0 {
            let cached = self
                .paging
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .best(query_key, offset);
            if let Some((o, s)) = cached {
                at = o;
                paging = s;
            }
            if offset - at > MAX_SKIP {
                return Err(Error::Unsupported("offset".into()));
            }
            // Read and drop rows until `offset`.
            while at < offset {
                let want = (offset - at).min(10_000) as i32;
                stmt.set_page_size(want);
                let (result, response) = session
                    .query_single_page(stmt.clone(), &[], paging.clone())
                    .await
                    .map_err(|e| error::from_execution(&e, text))?;
                let got = result.into_rows_result().map(|r| r.rows_num()).unwrap_or(0) as u64;
                at += got;
                match response.into_paging_control_flow() {
                    std::ops::ControlFlow::Break(()) => {
                        // Fewer rows than the offset: an empty page.
                        return self.empty_page(session, text, sink, started).await;
                    }
                    std::ops::ControlFlow::Continue(next) => {
                        paging = next;
                        self.paging.lock().unwrap_or_else(|e| e.into_inner()).put(
                            query_key.to_string(),
                            at,
                            paging.clone(),
                        );
                    }
                }
            }
        }
        let pending = self
            .page_loop(
                session,
                stmt,
                paging,
                Some(limit),
                batch,
                true,
                sink,
                started,
                true,
                text,
            )
            .await?;
        if let Some(p) = pending {
            self.paging.lock().unwrap_or_else(|e| e.into_inner()).put(
                query_key.to_string(),
                offset + limit,
                p.state,
            );
        }
        Ok(())
    }

    /// An empty result with the columns of `text` (offset beyond the end).
    async fn empty_page(
        &self,
        session: &std::sync::Arc<scylla::client::session::Session>,
        text: &str,
        sink: &mut dyn ResultSink,
        started: Instant,
    ) -> Result<()> {
        let mut stmt = Statement::new(text.to_string());
        stmt.set_page_size(1);
        let (result, _) = session
            .query_single_page(stmt, &[], PagingState::start())
            .await
            .map_err(|e| error::from_execution(&e, text))?;
        if let Ok(rows) = result.into_rows_result() {
            let metas = rows
                .column_specs()
                .iter()
                .map(|c| {
                    let t = CqlType::from_column_type(c.typ());
                    ColumnMeta {
                        name: c.name().to_string(),
                        type_name: t.to_string(),
                        value_kind: t.value_kind(),
                        nullable: None,
                    }
                })
                .collect();
            sink.on_columns(metas)?;
        }
        sink.on_statement_end(StatementEnd {
            outcome: EndOutcome::Rows {
                row_count: 0,
                truncated: false,
            },
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    /// The `SELECT` that streams a whole table (export, backup).
    pub(crate) async fn stream_select(&self, req: &TableStreamRequest) -> Result<String> {
        let snap = self.snapshot().await?;
        let (ks, name) = relation_of(&parse_path(&req.table)?)?;
        let t = snap.relation(&ks, &name)?;
        let w = build_where(t, &req.filters, req.raw_where.as_deref())?;
        if w.matches_nothing {
            // An impossible restriction on a key column.
            let first = t
                .key_columns()
                .first()
                .map(|c| quote_ident(&c.name))
                .ok_or_else(|| Error::Invalid("table without key".into()))?;
            return Ok(format!(
                "SELECT {} FROM {} WHERE {first} IN ()",
                if req.columns.is_empty() {
                    "*".to_string()
                } else {
                    req.columns
                        .iter()
                        .map(|c| quote_ident(c))
                        .collect::<Vec<_>>()
                        .join(", ")
                },
                qualified(&ks, &name)
            ));
        }
        select_text(&ks, t, &req.columns, &w, w.needs_filtering || w.raw)
    }

    /// Insert rows (prepared-style batches of independent statements, `BULK_CONCURRENCY` at a
    /// time).
    pub(crate) async fn bulk(
        &self,
        req: &BulkInsertRequest,
        consistency: Consistency,
        serial: Option<SerialConsistency>,
    ) -> Result<BulkInsertResult> {
        if self.pool.settings.read_only {
            return Err(Error::ReadOnlyViolation {
                message: "the connection is read-only".into(),
                position: None,
            });
        }
        let snap = self.snapshot().await?;
        let (ks, name) = relation_of(&parse_path(&req.table)?)?;
        let t = snap.relation(&ks, &name)?;
        if t.is_view || t.is_virtual || t.is_counter() {
            return Err(Error::Unsupported(
                "bulk insert into a view, virtual table or counter table".into(),
            ));
        }
        let cols: Vec<&crate::schema::ColumnInfo> = req
            .columns
            .iter()
            .map(|c| column(t, c))
            .collect::<Result<_>>()?;
        for k in t.key_columns() {
            if !req.columns.contains(&k.name) {
                return Err(Error::Invalid(format!(
                    "the primary key column `{}` is missing from the inserted columns",
                    k.name
                )));
            }
        }
        let lwt = !matches!(req.on_conflict, OnConflict::Update);
        let head = format!(
            "INSERT INTO {} ({}) VALUES (",
            qualified(&ks, &name),
            req.columns
                .iter()
                .map(|c| quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut statements = Vec::with_capacity(req.rows.len());
        for (i, row) in req.rows.iter().enumerate() {
            if row.len() != cols.len() {
                return Err(Error::Invalid(format!(
                    "row {i} has {} values for {} columns",
                    row.len(),
                    cols.len()
                )));
            }
            let lits = row
                .iter()
                .zip(&cols)
                .map(|(v, c)| match v {
                    EditValue::Value(v) => literal(v, &c.ty),
                    EditValue::Text(s) => literal_from_text(s, &c.ty),
                    EditValue::Default => Ok("null".to_string()),
                })
                .collect::<Result<Vec<_>>>()
                .map_err(|e| Error::Invalid(format!("row {i}: {e}")))?;
            statements.push(format!(
                "{head}{}){}",
                lits.join(", "),
                if lwt { " IF NOT EXISTS" } else { "" }
            ));
        }
        if lwt && !statements.is_empty() {
            tracing::warn!(
                "bulk insert with IF NOT EXISTS uses lightweight transactions and is slow"
            );
        }
        let session = self.pool.base();
        let mut result = BulkInsertResult::default();
        let mut stream = futures::stream::iter(statements.into_iter().map(|text| {
            let session = session.clone();
            async move {
                let mut stmt = Statement::new(text.clone());
                stmt.set_consistency(consistency);
                stmt.set_serial_consistency(serial);
                let r = session
                    .query_unpaged(stmt, &[])
                    .await
                    .map_err(|e| error::from_execution(&e, &text))?;
                Ok::<Option<bool>, Error>(if lwt { applied(r) } else { None })
            }
        }))
        .buffer_unordered(BULK_CONCURRENCY);
        while let Some(r) = stream.next().await {
            match r? {
                None | Some(true) => result.inserted += 1,
                Some(false) => match req.on_conflict {
                    OnConflict::Skip => result.skipped += 1,
                    _ => {
                        return Err(Error::Query {
                            message: format!(
                                "a row with the same primary key already exists in {ks}.{name}"
                            ),
                            position: None,
                        });
                    }
                },
            }
        }
        Ok(result)
    }
}

/// Column metadata of a table (for an empty result).
pub(crate) fn columns_of(t: &TableInfo) -> Vec<ColumnMeta> {
    t.columns
        .iter()
        .map(|c| ColumnMeta {
            name: c.name.clone(),
            type_name: c.ty.to_string(),
            value_kind: c.ty.value_kind(),
            nullable: Some(!c.is_key()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ColKind, ColumnInfo};

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
                c("ts", ColKind::Clustering, "timestamp"),
                c("name", ColKind::Regular, "text"),
                c("tags", ColKind::Regular, "set<text>"),
            ],
            ..TableInfo::default()
        }
    }

    fn f(col: &str, op: FilterOp, values: &[&str]) -> ColumnFilter {
        ColumnFilter {
            column: col.into(),
            op,
            values: values.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn where_on_key_and_regular_columns() {
        let t = table();
        let w = build_where(&t, &[f("id", FilterOp::Eq, &["5"])], None).unwrap();
        assert_eq!(w.clause, " WHERE id = 5");
        assert!(!w.needs_filtering);
        let w = build_where(
            &t,
            &[
                f("name", FilterOp::Eq, &["o'k"]),
                f("id", FilterOp::In, &["1", "2"]),
            ],
            Some("ts > '2020-01-01';"),
        )
        .unwrap();
        assert_eq!(
            w.clause,
            " WHERE name = 'o''k' AND id IN (1, 2) AND ts > '2020-01-01'"
        );
        assert!(w.needs_filtering && w.raw);
    }

    #[test]
    fn where_text_operators() {
        let t = table();
        let w = build_where(&t, &[f("name", FilterOp::StartsWith, &["ab"])], None).unwrap();
        assert_eq!(w.clause, " WHERE name LIKE 'ab%'");
        let w = build_where(&t, &[f("tags", FilterOp::Contains, &["x"])], None).unwrap();
        assert_eq!(w.clause, " WHERE tags CONTAINS 'x'");
        assert!(build_where(&t, &[f("name", FilterOp::Contains, &["a%"])], None).is_err());
        assert!(build_where(&t, &[f("id", FilterOp::Contains, &["a"])], None).is_err());
    }

    #[test]
    fn where_unsupported_and_invalid() {
        let t = table();
        for op in [
            FilterOp::Ne,
            FilterOp::NotContains,
            FilterOp::IsNull,
            FilterOp::IsNotNull,
        ] {
            let e = build_where(&t, &[f("name", op, &["x"])], None).unwrap_err();
            assert_eq!(e.kind(), "unsupported");
        }
        assert_eq!(
            build_where(&t, &[f("nope", FilterOp::Eq, &["x"])], None)
                .unwrap_err()
                .kind(),
            "invalid"
        );
        assert_eq!(
            build_where(&t, &[f("id", FilterOp::Eq, &["abc"])], None)
                .unwrap_err()
                .kind(),
            "invalid"
        );
        assert!(
            build_where(&t, &[f("id", FilterOp::In, &[])], None)
                .unwrap()
                .matches_nothing
        );
    }

    #[test]
    fn select_statement() {
        let t = table();
        let w = build_where(&t, &[], None).unwrap();
        assert_eq!(
            select_text("ks", &t, &[], &w, false).unwrap(),
            "SELECT * FROM ks.t"
        );
        assert_eq!(
            select_text("ks", &t, &["id".into(), "name".into()], &w, true).unwrap(),
            "SELECT id, name FROM ks.t ALLOW FILTERING"
        );
        assert!(select_text("ks", &t, &["zz".into()], &w, false).is_err());
    }
}
