//! Requests for browsing table data with filters, sorting and paging.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ObjectKind, ObjectPath};

/// Comparison operator of a [`ColumnFilter`].
///
/// Semantics every driver must follow:
///
/// * `Contains`, `NotContains`, `StartsWith` and `EndsWith` are **case-insensitive** and match on
///   the text form of the column; the user's `%`/`_` (and escape characters) are literal.
///   Accent sensitivity, and `Eq`/`Ne`/ordering comparisons on strings, follow the database's
///   collation and may legitimately differ between databases.
/// * `NotContains` follows SQL `NOT LIKE`: rows where the column is `NULL` are excluded too.
/// * `In` with no values matches no row.
/// * `IsNull`/`IsNotNull` ignore `values`.
///
/// For [`crate::Session::fetch_table`] the driver always appends the primary key (when the table
/// has one; otherwise a unique `NOT NULL` key) to the end of `ORDER BY`, even if `sort` is
/// empty, so that `OFFSET` paging is deterministic. [`crate::Error::Query`] errors from
/// `fetch_table` and `count_table` have `position: None` because the SQL is generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FilterOp {
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// Substring match (`LIKE '%v%'`, wildcards escaped).
    Contains,
    /// Negated substring match.
    NotContains,
    /// Prefix match.
    StartsWith,
    /// Suffix match.
    EndsWith,
    /// `IS NULL` (no values).
    IsNull,
    /// `IS NOT NULL` (no values).
    IsNotNull,
    /// `IN (...)` with every entry of `values`.
    In,
}

/// One filter condition on a column. Conditions of a request are combined with `AND`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnFilter {
    /// Column name (unquoted).
    pub column: String,
    /// Operator.
    pub op: FilterOp,
    /// Operands as user-typed strings: one for comparisons, any number for `In`, none for the
    /// null tests.
    #[serde(default)]
    pub values: Vec<String>,
}

/// Sort criterion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SortKey {
    /// Column name (unquoted).
    pub column: String,
    /// Descending order when true.
    #[serde(default)]
    pub descending: bool,
}

/// Request for a page of table rows, see [`crate::Session::fetch_table`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableDataRequest {
    /// Identifier used to cancel the fetch via [`crate::Session::cancel`].
    pub query_id: Uuid,
    /// Table or view to read.
    pub table: ObjectPath,
    /// Structured filters (`AND`ed).
    #[serde(default)]
    pub filters: Vec<ColumnFilter>,
    /// User-typed `WHERE` clause body (without the `WHERE` keyword), appended verbatim.
    #[serde(default)]
    pub raw_where: Option<String>,
    /// Sort order.
    #[serde(default)]
    pub sort: Vec<SortKey>,
    /// Rows to skip.
    #[serde(default)]
    pub offset: u64,
    /// Maximum rows to return.
    pub limit: u64,
    /// Rows per streamed batch; the driver picks a default when `None`.
    #[serde(default)]
    pub batch_size: Option<u32>,
}

/// Request for the number of rows matching a filter, see [`crate::Session::count_table`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableCountRequest {
    /// Table or view to count.
    pub table: ObjectPath,
    /// Structured filters (`AND`ed).
    #[serde(default)]
    pub filters: Vec<ColumnFilter>,
    /// User-typed `WHERE` clause body, appended verbatim.
    #[serde(default)]
    pub raw_where: Option<String>,
    /// Optional identifier so that [`crate::Session::cancel`] can abort a slow count.
    #[serde(default)]
    pub query_id: Option<Uuid>,
}

/// Request for a server-side search of objects by name, see [`crate::Session::search_objects`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectSearchRequest {
    /// Text typed by the user: case-insensitive substring match on the object **name**.
    pub query: String,
    /// Where to search: the root, a `Database` node or a `Schema` node (with or without
    /// `Folder` segments). Only objects inside are returned.
    pub scope: ObjectPath,
    /// Kinds to return; empty means the default set: `Table`, `View`, `MaterializedView`,
    /// `Function`, `Procedure`, `Sequence`.
    #[serde(default)]
    pub kinds: Vec<ObjectKind>,
    /// Maximum number of results (drivers clamp it to `1..=1000`).
    pub limit: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectKind;
    use serde_json::json;

    #[test]
    fn search_request_shape() {
        let req: ObjectSearchRequest = serde_json::from_value(json!({
            "query": "us",
            "scope": {"segments": []},
            "limit": 50
        }))
        .unwrap();
        assert!(req.kinds.is_empty() && req.scope.is_root());
        let req = ObjectSearchRequest {
            kinds: vec![ObjectKind::MaterializedView],
            ..req
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["kinds"][0], "materializedView");
        assert_eq!(json["limit"], 50);
        assert_eq!(
            serde_json::from_value::<ObjectSearchRequest>(json).unwrap(),
            req
        );
    }

    #[test]
    fn request_roundtrip_and_defaults() {
        let req: TableDataRequest = serde_json::from_value(json!({
            "queryId": "67e55044-10b1-426f-9247-bb680e5fe0c8",
            "table": {"segments": [{"kind": "table", "name": "t"}]},
            "filters": [{"column": "name", "op": "notContains", "values": ["x"]}],
            "limit": 100
        }))
        .unwrap();
        assert_eq!(req.offset, 0);
        assert!(req.sort.is_empty() && req.raw_where.is_none());
        assert_eq!(req.filters[0].op, FilterOp::NotContains);
        assert_eq!(req.table.last().unwrap().kind, ObjectKind::Table);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["filters"][0]["op"], "notContains");
        assert_eq!(
            serde_json::from_value::<TableDataRequest>(json).unwrap(),
            req
        );
        let null_op: ColumnFilter =
            serde_json::from_value(json!({"column": "a", "op": "isNull"})).unwrap();
        assert!(null_op.values.is_empty());
    }
}
