//! Table reads (`fetch_table`, `count_table`, streaming for export) and object search.

use dbstudio_core::{
    CollectingSink, Error, ExecSummary, ObjectKind, ObjectNode, ObjectPath, ObjectSearchRequest,
    Result, ResultSink, StatementOutcome, TableCountRequest, TableDataRequest, TableStreamRequest,
    Value,
};

use crate::catalog;
use crate::conn::Conn;
use crate::exec::RunParams;
use crate::session::{DEFAULT_BATCH, MssqlSession};
use crate::sql::{ObjRef, lit, object_path, order_by, parse_object, quote, where_clause};
use crate::value::str_at;

/// Check that `raw` parses as a `WHERE` expression of `table` (syntax only, nothing runs).
pub(crate) async fn check_raw_where(conn: &mut Conn, table: &str, raw: &str) -> Result<()> {
    conn.run("SET PARSEONLY ON").await?;
    let check = conn
        .run(&format!("SELECT 1 FROM {table} WHERE (\n{raw}\n)"))
        .await;
    let off = conn.run("SET PARSEONLY OFF").await;
    if off.is_err() {
        conn.broken = true;
    }
    check.map_err(|e| match e {
        Error::Query { message, .. } => Error::Query {
            message,
            position: None,
        },
        other => other,
    })?;
    off
}

/// Candidate row identities: the primary key first, then every unique index whose columns are
/// all `NOT NULL` (and that is not filtered or disabled).
pub(crate) async fn candidate_keys(conn: &mut Conn, obj: &ObjRef) -> Result<Vec<Vec<String>>> {
    let filter = catalog::one(&obj.schema, &obj.name);
    let key = (obj.schema.clone(), obj.name.clone());
    let idx = catalog::indexes(conn, &filter)
        .await?
        .remove(&key)
        .unwrap_or_default();
    let cols = catalog::columns(conn, &filter)
        .await?
        .remove(&key)
        .unwrap_or_default();
    let nullable = |name: &str| {
        cols.iter()
            .find(|c| c.name == name)
            .is_none_or(|c| c.nullable)
    };
    let names = |i: &catalog::Idx| i.keys.iter().map(|(c, _)| c.clone()).collect::<Vec<_>>();
    let mut out: Vec<Vec<String>> = idx.iter().filter(|i| i.primary).map(names).collect();
    out.extend(
        idx.iter()
            .filter(|i| {
                !i.primary
                    && i.unique
                    && i.filter.is_none()
                    && !i.disabled
                    && matches!(i.type_id, 1 | 2)
                    && !i.keys.is_empty()
                    && i.keys.iter().all(|(c, _)| !nullable(c))
            })
            .map(names),
    );
    Ok(out)
}

/// Key columns used as paging tie-breaker and as row identity: the primary key, else the first
/// unique index whose columns are all `NOT NULL`.
pub(crate) async fn key_columns(conn: &mut Conn, obj: &ObjRef) -> Result<Vec<String>> {
    Ok(candidate_keys(conn, obj)
        .await?
        .into_iter()
        .next()
        .unwrap_or_default())
}

fn cache_id(db: &str, obj: &ObjRef) -> (String, String, String) {
    (db.to_string(), obj.schema.clone(), obj.name.clone())
}

/// How a column is read so that huge cells never travel in full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LobKind {
    None,
    /// `varchar(max)` / `nvarchar(max)`.
    TextMax,
    Xml,
    Text,
    NText,
    /// `varbinary(max)`.
    BytesMax,
    Image,
}

/// Key and column read plan of a table, cached for a short time.
#[derive(Debug, Clone, Default)]
pub(crate) struct TableInfo {
    pub key: Vec<String>,
    pub columns: Vec<(String, LobKind)>,
}

impl TableInfo {
    /// Select list reading only the first `limit + 1` characters/bytes of every large column
    /// (the value decoder then marks the cell truncated). `*` when nothing is large.
    pub fn select_list(&self, only: &[String], limit: usize) -> String {
        let n = limit + 1;
        let expr = |c: &str, k: LobKind| {
            let q = quote(c);
            match k {
                LobKind::None => q,
                LobKind::TextMax => format!("LEFT({q}, {n}) AS {q}"),
                LobKind::Xml => format!("LEFT(CAST({q} AS NVARCHAR(MAX)), {n}) AS {q}"),
                LobKind::Text => format!("LEFT(CAST({q} AS VARCHAR(MAX)), {n}) AS {q}"),
                LobKind::NText => format!("LEFT(CAST({q} AS NVARCHAR(MAX)), {n}) AS {q}"),
                LobKind::BytesMax => format!("SUBSTRING({q}, 1, {n}) AS {q}"),
                LobKind::Image => format!("SUBSTRING(CAST({q} AS VARBINARY(MAX)), 1, {n}) AS {q}"),
            }
        };
        let any_lob = self.columns.iter().any(|(_, k)| *k != LobKind::None);
        if self.columns.is_empty() || (!any_lob && only.is_empty()) {
            return if only.is_empty() {
                "*".to_string()
            } else {
                only.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", ")
            };
        }
        if only.is_empty() {
            return self
                .columns
                .iter()
                .map(|(c, k)| expr(c, *k))
                .collect::<Vec<_>>()
                .join(", ");
        }
        only.iter()
            .map(|c| {
                let k = self
                    .columns
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(c))
                    .map_or(LobKind::None, |(_, k)| *k);
                expr(c, k)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn lob_kind(type_name: &str, max_length: i64) -> LobKind {
    match type_name.to_ascii_lowercase().as_str() {
        "varchar" | "nvarchar" if max_length == -1 => LobKind::TextMax,
        "varbinary" if max_length == -1 => LobKind::BytesMax,
        "xml" => LobKind::Xml,
        "text" => LobKind::Text,
        "ntext" => LobKind::NText,
        "image" => LobKind::Image,
        _ => LobKind::None,
    }
}

/// Read key and columns from the catalog.
pub(crate) async fn load_table_info(conn: &mut Conn, obj: &ObjRef) -> TableInfo {
    let key = key_columns(conn, obj).await.unwrap_or_default();
    let rows = conn
        .rows(&format!(
            "SELECT c.name, CASE WHEN t.is_user_defined = 1 THEN \
             (SELECT bt.name FROM sys.types bt WHERE bt.user_type_id = t.system_type_id) ELSE t.name END, \
             c.max_length FROM sys.columns c JOIN sys.types t ON t.user_type_id = c.user_type_id \
             WHERE c.object_id = OBJECT_ID({}) ORDER BY c.column_id",
            lit(&obj.qualified())
        ))
        .await
        .unwrap_or_default();
    let columns = rows
        .iter()
        .map(|r| {
            (
                str_at(r, 0),
                lob_kind(&str_at(r, 1), crate::value::int_at(r, 2)),
            )
        })
        .collect();
    TableInfo { key, columns }
}

pub(crate) async fn table_info(
    sess: &MssqlSession,
    conn: &mut Conn,
    db: &str,
    obj: &ObjRef,
) -> TableInfo {
    let id = cache_id(db, obj);
    if let Some(k) = sess.cached_info(&id) {
        return k;
    }
    let k = load_table_info(conn, obj).await;
    sess.cache_info(id, k.clone());
    k
}

/// Query errors of generated SQL carry no position.
fn strip_position(e: Error) -> Error {
    match e {
        Error::Query { message, .. } => Error::Query {
            message,
            position: None,
        },
        other => other,
    }
}

fn target_of(sess: &MssqlSession, table: &ObjectPath) -> Result<(String, ObjRef)> {
    let obj = parse_object(table)?;
    if !matches!(obj.kind, ObjectKind::Table | ObjectKind::View) {
        return Err(Error::NotFound(format!(
            "{} is not a table or view",
            obj.qualified()
        )));
    }
    let db = obj
        .db
        .clone()
        .unwrap_or_else(|| sess.login_db().to_string());
    Ok((db, obj))
}

pub async fn fetch_table(
    sess: &MssqlSession,
    req: TableDataRequest,
    sink: &mut dyn ResultSink,
) -> Result<ExecSummary> {
    let (db, obj) = target_of(sess, &req.table)?;
    let mut conn = sess.shared.acquire(&db).await?;
    let table = obj.qualified();
    let info = table_info(sess, &mut conn, &db, &obj).await;
    let key = info.key.clone();
    let list = info.select_list(&[], conn.lob_limit);
    let prepared = async {
        let where_ = where_clause(&req.filters, req.raw_where.as_deref())?;
        if let Some(raw) = req
            .raw_where
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            check_raw_where(&mut conn, &table, raw).await?;
        }
        Ok::<_, Error>(where_)
    }
    .await;
    let where_ = match prepared {
        Ok(w) => w,
        Err(e) => {
            conn.release();
            return Err(e);
        }
    };
    let sql = if req.limit == 0 {
        format!("SELECT TOP (0) {list} FROM {table}{where_}")
    } else {
        format!(
            "SELECT {list} FROM {table}{where_}{} OFFSET {} ROWS FETCH NEXT {} ROWS ONLY",
            order_by(&req.sort, &key),
            req.offset,
            req.limit
        )
    };
    let params = RunParams {
        max_rows: None,
        batch_size: req
            .batch_size
            .map(|b| b as usize)
            .unwrap_or(DEFAULT_BATCH)
            .clamp(1, 100_000),
        timeout: None,
    };
    // READ COMMITTED explicitly (design 3.1), whatever the pooled connection had.
    let mut conn = conn;
    conn.run("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .await?;
    let r = run_on(sess, conn, Some(req.query_id), &sql, &params, sink).await;
    r.map_err(strip_position)
}

/// Run `sql` on an already acquired pooled connection (registered for cancel by `id`).
async fn run_on(
    sess: &MssqlSession,
    mut conn: crate::conn::Pooled,
    id: Option<uuid::Uuid>,
    sql: &str,
    params: &RunParams,
    sink: &mut dyn ResultSink,
) -> Result<ExecSummary> {
    use std::sync::Arc;
    let ctl = Arc::new(crate::exec::Control::default());
    let _reg = sess.register(id, &ctl, &conn);
    let r = crate::exec::run_script(
        &mut conn,
        sql,
        params,
        &ctl,
        None,
        &mut crate::exec::SinkOut(sink),
    )
    .await;
    sess.finish(conn, None).await;
    r
}

pub async fn count_table(sess: &MssqlSession, req: TableCountRequest) -> Result<u64> {
    let (db, obj) = target_of(sess, &req.table)?;
    let mut conn = sess.shared.acquire(&db).await?;
    let table = obj.qualified();
    let where_ = match where_clause(&req.filters, req.raw_where.as_deref()) {
        Ok(w) => w,
        Err(e) => {
            conn.release();
            return Err(e);
        }
    };
    if let Some(raw) = req
        .raw_where
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        if let Err(e) = check_raw_where(&mut conn, &table, raw).await {
            conn.release();
            return Err(e);
        }
    }
    let sql = format!("SELECT COUNT_BIG(*) FROM {table}{where_}");
    let params = RunParams {
        max_rows: None,
        batch_size: 10,
        timeout: None,
    };
    let mut sink = CollectingSink::new();
    run_on(sess, conn, req.query_id, &sql, &params, &mut sink)
        .await
        .map_err(strip_position)?;
    let results = sink.into_results();
    match results.first().map(|r| &r.outcome) {
        Some(StatementOutcome::Rows(rs)) => match rs.rows.first().and_then(|r| r.first()) {
            Some(Value::Int(n)) => Ok((*n).max(0) as u64),
            other => Err(Error::Other(format!("unexpected count value {other:?}"))),
        },
        _ => Err(Error::Other("the count query returned no rows".into())),
    }
}

/// SQL of a whole-table read for export (no paging).
pub(crate) fn stream_sql(
    obj: &ObjRef,
    req: &TableStreamRequest,
    info: &TableInfo,
    lob_limit: usize,
) -> Result<String> {
    let cols = info.select_list(&req.columns, lob_limit);
    let where_ = where_clause(&req.filters, req.raw_where.as_deref())?;
    let order = if req.order_by_key && !info.key.is_empty() {
        order_by(&[], &info.key)
    } else {
        String::new()
    };
    Ok(format!(
        "SELECT {cols} FROM {}{where_}{order}",
        obj.qualified()
    ))
}

/// Default kinds of an object search.
const DEFAULT_KINDS: [ObjectKind; 5] = [
    ObjectKind::Table,
    ObjectKind::View,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Sequence,
];

fn type_codes(k: ObjectKind) -> Option<&'static str> {
    Some(match k {
        ObjectKind::Table => "'U'",
        ObjectKind::View => "'V'",
        ObjectKind::Function => "'FN','IF','TF','FS','FT'",
        ObjectKind::Procedure => "'P','PC','RF','X'",
        ObjectKind::Sequence => "'SO'",
        ObjectKind::Synonym => "'SN'",
        _ => return None,
    })
}

fn kind_of_code(code: &str) -> ObjectKind {
    match code.trim() {
        "U" => ObjectKind::Table,
        "V" => ObjectKind::View,
        "P" | "PC" | "RF" | "X" => ObjectKind::Procedure,
        "SO" => ObjectKind::Sequence,
        "SN" => ObjectKind::Synonym,
        _ => ObjectKind::Function,
    }
}

/// Search by name in one database (the session's default when the scope is the root).
pub async fn search_objects(
    sess: &MssqlSession,
    req: ObjectSearchRequest,
) -> Result<Vec<ObjectNode>> {
    let q = req.query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let limit = req.limit.clamp(1, 1000);
    let db = sess.database_of(&req.scope);
    let schema = req
        .scope
        .segments
        .iter()
        .find(|s| s.kind == ObjectKind::Schema)
        .map(|s| s.name.clone());
    let kinds: Vec<ObjectKind> = if req.kinds.is_empty() {
        DEFAULT_KINDS.to_vec()
    } else {
        req.kinds.clone()
    };
    let mut codes: Vec<&str> = kinds.iter().filter_map(|k| type_codes(*k)).collect();
    codes.dedup();
    let with_types = kinds.contains(&ObjectKind::Type);
    if codes.is_empty() && !with_types {
        return Ok(Vec::new());
    }
    let pattern = crate::sql::like_escape(q);
    let name_match = |col: &str| {
        format!(
            "{col} COLLATE {} LIKE {} ESCAPE N'\\'",
            crate::sql::CI_COLLATION,
            lit(&format!("%{pattern}%"))
        )
    };
    let rank = |col: &str| {
        format!(
            "CASE WHEN {col} COLLATE {c} = {exact} THEN 0 WHEN {col} COLLATE {c} LIKE {prefix} ESCAPE N'\\' THEN 1 ELSE 2 END",
            c = crate::sql::CI_COLLATION,
            exact = lit(q),
            prefix = lit(&format!("{pattern}%"))
        )
    };
    let schema_filter = schema
        .as_ref()
        .map(|s| format!(" AND SCHEMA_NAME(o.schema_id) = {}", lit(s)))
        .unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    if !codes.is_empty() {
        parts.push(format!(
            "SELECT SCHEMA_NAME(o.schema_id) AS s, o.name AS n, o.type AS t, {r} AS r \
             FROM sys.objects o WHERE o.type IN ({}) AND {}{schema_filter}",
            codes.join(","),
            name_match("o.name"),
            r = rank("o.name"),
        ));
    }
    if with_types {
        let tf = schema
            .as_ref()
            .map(|s| format!(" AND SCHEMA_NAME(t.schema_id) = {}", lit(s)))
            .unwrap_or_default();
        parts.push(format!(
            "SELECT SCHEMA_NAME(t.schema_id), t.name, 'TY', {r} FROM sys.types t \
             WHERE t.is_user_defined = 1 AND {}{tf}",
            name_match("t.name"),
            r = rank("t.name"),
        ));
    }
    let sql = format!(
        "SELECT TOP ({limit}) s, n, t FROM ({}) x ORDER BY r, n, s",
        parts.join(" UNION ALL ")
    );
    let mut conn = sess.shared.acquire_catalog(&db).await?;
    let rows = conn.rows(&sql).await?;
    conn.release();
    let show_db = req.scope.is_root();
    Ok(rows
        .iter()
        .map(|r| {
            let s = str_at(r, 0);
            let name = str_at(r, 1);
            let code = str_at(r, 2);
            let (kind, path) = if code.trim() == "TY" {
                (
                    ObjectKind::Type,
                    object_path(&db, &s, ObjectKind::Type, &name),
                )
            } else {
                let kind = kind_of_code(&code);
                (kind, object_path(&db, &s, kind, &name))
            };
            ObjectNode {
                path,
                kind,
                name,
                has_children: matches!(kind, ObjectKind::Table | ObjectKind::View),
                detail: Some(if show_db { format!("{db}.{s}") } else { s }),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{ColumnFilter, FilterOp};

    #[test]
    fn stream_sql_shapes() {
        let obj = ObjRef {
            db: None,
            schema: "dbo".into(),
            name: "t".into(),
            kind: ObjectKind::Table,
            child: None,
        };
        let req = TableStreamRequest {
            query_id: uuid::Uuid::nil(),
            table: ObjectPath::root(),
            columns: vec!["a".into(), "b]".into()],
            filters: vec![ColumnFilter {
                column: "a".into(),
                op: FilterOp::IsNull,
                values: vec![],
            }],
            raw_where: None,
            order_by_key: true,
            batch_size: None,
        };
        let info = TableInfo {
            key: vec!["id".into()],
            columns: vec![],
        };
        assert_eq!(
            stream_sql(&obj, &req, &info, 100).unwrap(),
            "SELECT [a], [b]]] FROM [dbo].[t] WHERE ([a] IS NULL) ORDER BY [id]"
        );
        let req = TableStreamRequest {
            columns: vec![],
            filters: vec![],
            order_by_key: false,
            ..req
        };
        assert_eq!(
            stream_sql(&obj, &req, &TableInfo::default(), 100).unwrap(),
            "SELECT * FROM [dbo].[t]"
        );
        let info = TableInfo {
            key: vec![],
            columns: vec![
                ("a".into(), LobKind::None),
                ("b".into(), LobKind::TextMax),
                ("c".into(), LobKind::Image),
            ],
        };
        assert_eq!(
            info.select_list(&[], 9),
            "[a], LEFT([b], 10) AS [b], SUBSTRING(CAST([c] AS VARBINARY(MAX)), 1, 10) AS [c]"
        );
        assert_eq!(info.select_list(&["b".into()], 9), "LEFT([b], 10) AS [b]");
    }

    #[test]
    fn search_kinds() {
        assert_eq!(kind_of_code("U "), ObjectKind::Table);
        assert_eq!(kind_of_code("IF"), ObjectKind::Function);
        assert_eq!(kind_of_code("PC"), ObjectKind::Procedure);
        assert!(type_codes(ObjectKind::Column).is_none());
    }
}
