//! Table metadata returned by [`crate::Session::table_details`].

use serde::{Deserialize, Serialize};

use crate::{ObjectKind, ObjectPath, ValueKind};

/// Full description of a table or view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableDetails {
    /// Path of the table.
    pub path: ObjectPath,
    /// Table, view or materialized view.
    pub kind: ObjectKind,
    /// Object comment.
    pub comment: Option<String>,
    /// Columns in ordinal order.
    pub columns: Vec<ColumnDetail>,
    /// Indexes, including the primary key index.
    pub indexes: Vec<IndexDetail>,
    /// Outgoing foreign keys.
    pub foreign_keys: Vec<ForeignKeyDetail>,
    /// Estimated row count from statistics.
    pub estimated_rows: Option<u64>,
    /// Total size on disk in bytes.
    pub size_bytes: Option<u64>,
}

/// A table column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnDetail {
    /// Column name.
    pub name: String,
    /// One-based position in the table.
    pub ordinal: u32,
    /// Native type name (e.g. `varchar(20)`).
    pub type_name: String,
    /// Closest [`ValueKind`] cells of this column are decoded into.
    pub value_kind: ValueKind,
    /// Whether NULL is allowed.
    pub nullable: bool,
    /// Default expression, verbatim.
    pub default: Option<String>,
    /// Column comment.
    pub comment: Option<String>,
    /// Part of the primary key.
    pub primary_key: bool,
    /// Extra attributes such as `auto_increment`, `identity` or `generated`.
    pub extra: Option<String>,
}

/// An index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexDetail {
    /// Index name.
    pub name: String,
    /// Indexed column names in order (plain names, no quoting, prefix lengths or sort markers).
    /// An expression key part is rendered as `(expr)`.
    pub columns: Vec<String>,
    /// Enforces uniqueness.
    pub unique: bool,
    /// Backs the primary key.
    pub primary: bool,
    /// Access method (`btree`, `hash`, `gin`...).
    pub method: Option<String>,
    /// Full `CREATE INDEX` definition when available.
    pub definition: Option<String>,
    /// Partial index (has a predicate): covers only some rows, so it can never identify a row.
    #[serde(default)]
    pub partial: bool,
}

/// A foreign key constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignKeyDetail {
    /// Constraint name.
    pub name: String,
    /// Local columns.
    pub columns: Vec<String>,
    /// Referenced table.
    pub referenced_table: ObjectPath,
    /// Referenced columns, aligned with `columns`.
    pub referenced_columns: Vec<String>,
    /// `ON UPDATE` action (`CASCADE`, `NO ACTION`...).
    pub on_update: Option<String>,
    /// `ON DELETE` action.
    pub on_delete: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_details_serializes_camel_case() {
        let d = TableDetails {
            path: ObjectPath::root().child(ObjectKind::Table, "t"),
            kind: ObjectKind::Table,
            comment: None,
            columns: vec![ColumnDetail {
                name: "id".into(),
                ordinal: 1,
                type_name: "int8".into(),
                value_kind: ValueKind::Int,
                nullable: false,
                default: None,
                comment: None,
                primary_key: true,
                extra: Some("identity".into()),
            }],
            indexes: vec![IndexDetail {
                name: "t_pkey".into(),
                columns: vec!["id".into()],
                unique: true,
                primary: true,
                method: Some("btree".into()),
                definition: None,
                partial: false,
            }],
            foreign_keys: vec![ForeignKeyDetail {
                name: "fk".into(),
                columns: vec!["a".into()],
                referenced_table: ObjectPath::root().child(ObjectKind::Table, "u"),
                referenced_columns: vec!["id".into()],
                on_update: None,
                on_delete: Some("CASCADE".into()),
            }],
            estimated_rows: Some(10),
            size_bytes: None,
        };
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["estimatedRows"], 10);
        assert_eq!(json["columns"][0]["typeName"], "int8");
        assert_eq!(json["columns"][0]["primaryKey"], true);
        assert_eq!(json["foreignKeys"][0]["referencedColumns"][0], "id");
        assert_eq!(serde_json::from_value::<TableDetails>(json).unwrap(), d);
    }
}
