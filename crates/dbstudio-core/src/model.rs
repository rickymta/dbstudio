//! Schema model (ERD, autocomplete), DDL generation, table streaming and bulk insert.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ColumnDetail, ColumnFilter, EditValue, ObjectKind, ObjectPath};

/// How much detail [`SchemaModelRequest`] asks for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModelDetail {
    /// PK/FK/unique columns only.
    Keys,
    /// All columns (autocomplete uses this without comments).
    #[default]
    Full,
}

/// Request for the schema model of a database or schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaModelRequest {
    /// Database or Schema node.
    pub scope: ObjectPath,
    /// Restrict to these tables; empty = all tables in scope.
    #[serde(default)]
    pub tables: Vec<ObjectPath>,
    /// Include views.
    #[serde(default)]
    pub include_views: bool,
    /// Include comments.
    #[serde(default)]
    pub include_comments: bool,
    /// Detail level.
    #[serde(default)]
    pub detail: ModelDetail,
    /// Mongo inference sample size (default 1000).
    #[serde(default)]
    pub sample_size: Option<u32>,
}

/// Tables and relations of a scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaModel {
    /// The scope requested.
    pub scope: ObjectPath,
    /// Tables.
    pub tables: Vec<TableModel>,
    /// Foreign-key relations.
    pub relations: Vec<Relation>,
    /// Inferred by sampling (Mongo).
    pub inferred: bool,
    /// More than 2000 tables existed: first 2000 by name.
    pub truncated: bool,
}

/// One table of a [`SchemaModel`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableModel {
    /// Full tree path including Folder segments.
    pub path: ObjectPath,
    /// Table, view...
    pub kind: ObjectKind,
    /// Schema name when the database has schemas.
    pub schema: Option<String>,
    /// Table name.
    pub name: String,
    /// Table comment.
    pub comment: Option<String>,
    /// Columns (Mongo: `typeName` `string|int32`, `extra` `seen 87%`).
    pub columns: Vec<ColumnDetail>,
    /// Primary key columns.
    pub primary_key: Vec<String>,
    /// Other unique keys.
    pub unique_keys: Vec<Vec<String>>,
    /// Estimated row count.
    pub estimated_rows: Option<u64>,
}

/// A foreign key (or inferred reference) between tables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Relation {
    /// Constraint name.
    pub name: String,
    /// Referencing table.
    pub from_table: ObjectPath,
    /// Referencing columns.
    pub from_columns: Vec<String>,
    /// Referenced table.
    pub to_table: ObjectPath,
    /// Referenced columns.
    pub to_columns: Vec<String>,
    /// `ON UPDATE` action.
    pub on_update: Option<String>,
    /// `ON DELETE` action.
    pub on_delete: Option<String>,
    /// Any FK column nullable ("zero or one").
    pub from_optional: bool,
    /// FK columns are themselves PK/unique.
    pub one_to_one: bool,
    /// Mongo heuristic (`fooId` -> `foos._id`).
    pub inferred: bool,
}

/// Request to generate DDL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DdlRequest {
    /// Database, Schema or one object.
    pub scope: ObjectPath,
    /// Restrict to these objects (tables chosen in export).
    #[serde(default)]
    pub objects: Vec<ObjectPath>,
    /// Options.
    #[serde(default)]
    pub options: DdlOptions,
}

/// DDL generation options. Default: everything true except `include_drop`, `if_not_exists`,
/// `include_grants`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DdlOptions {
    /// Emit `DROP` before `CREATE`.
    pub include_drop: bool,
    /// `IF NOT EXISTS` where supported.
    pub if_not_exists: bool,
    /// Indexes.
    pub include_indexes: bool,
    /// Foreign keys.
    pub include_foreign_keys: bool,
    /// Triggers.
    pub include_triggers: bool,
    /// Views.
    pub include_views: bool,
    /// Functions, procedures, packages.
    pub include_routines: bool,
    /// Sequences.
    pub include_sequences: bool,
    /// Comments.
    pub include_comments: bool,
    /// Grants.
    pub include_grants: bool,
    /// Qualify names with the schema.
    pub qualify_schema: bool,
}

impl Default for DdlOptions {
    fn default() -> Self {
        DdlOptions {
            include_drop: false,
            if_not_exists: false,
            include_indexes: true,
            include_foreign_keys: true,
            include_triggers: true,
            include_views: true,
            include_routines: true,
            include_sequences: true,
            include_comments: true,
            include_grants: false,
            qualify_schema: true,
        }
    }
}

/// Where a [`DdlItem`] goes relative to the data load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DdlSection {
    /// Before the data (schema, types, sequences, tables without FK, views, routines).
    PreData,
    /// After the data (indexes, FKs, triggers, setval, comments, grants).
    PostData,
}

/// Language of a [`DdlItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DdlLanguage {
    /// SQL.
    Sql,
    /// MongoDB shell commands.
    MongoShell,
}

/// One executable DDL unit. The order of the returned `Vec` is the creation order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DdlItem {
    /// Object the item creates.
    pub path: ObjectPath,
    /// Kind of the object.
    pub kind: ObjectKind,
    /// Before or after the data.
    pub section: DdlSection,
    /// One executable unit including its terminator (`;`; MSSQL: one batch without `GO`;
    /// Oracle PL/SQL: ends with `\n/`).
    pub sql: String,
    /// Language.
    pub language: DdlLanguage,
    /// Objects that must exist first.
    pub depends_on: Vec<ObjectPath>,
}

/// Request to stream a whole table through [`crate::Lease::stream_table`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableStreamRequest {
    /// Identifier used to cancel.
    pub query_id: Uuid,
    /// Table to read.
    pub table: ObjectPath,
    /// Columns to read; empty = all, in ordinal order.
    #[serde(default)]
    pub columns: Vec<String>,
    /// Filters (`AND`ed).
    #[serde(default)]
    pub filters: Vec<ColumnFilter>,
    /// User-typed `WHERE` body, appended verbatim.
    #[serde(default)]
    pub raw_where: Option<String>,
    /// Order by the key (deterministic output).
    #[serde(default)]
    pub order_by_key: bool,
    /// Rows per batch.
    #[serde(default)]
    pub batch_size: Option<u32>,
}

/// What to do when an inserted row conflicts with an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OnConflict {
    /// Fail.
    Error,
    /// Keep the existing row.
    Skip,
    /// Overwrite the existing row.
    Update,
}

/// Request of [`crate::Lease::bulk_insert`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkInsertRequest {
    /// Target table.
    pub table: ObjectPath,
    /// Columns, aligned with each row.
    pub columns: Vec<String>,
    /// Rows.
    pub rows: Vec<Vec<EditValue>>,
    /// Conflict policy.
    pub on_conflict: OnConflict,
    /// Conflict target columns; default: the primary key.
    #[serde(default)]
    pub conflict_columns: Vec<String>,
}

/// Counts returned by `bulk_insert`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkInsertResult {
    /// Rows inserted.
    pub inserted: u64,
    /// Rows updated.
    pub updated: u64,
    /// Rows skipped.
    pub skipped: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Value, ValueKind};
    use serde_json::json;

    fn path(name: &str) -> ObjectPath {
        ObjectPath::root()
            .child(ObjectKind::Database, "app")
            .child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, name)
    }

    #[test]
    fn request_defaults() {
        let r: SchemaModelRequest =
            serde_json::from_value(json!({"scope": {"segments": []}})).unwrap();
        assert!(r.tables.is_empty() && !r.include_views && !r.include_comments);
        assert_eq!(r.detail, ModelDetail::Full);
        assert_eq!(serde_json::to_value(ModelDetail::Keys).unwrap(), "keys");
        let o: DdlOptions = serde_json::from_value(json!({})).unwrap();
        assert_eq!(o, DdlOptions::default());
        assert!(!o.include_drop && !o.if_not_exists && !o.include_grants);
        assert!(o.include_indexes && o.include_foreign_keys && o.qualify_schema);
        let partial: DdlOptions = serde_json::from_value(json!({"includeDrop": true})).unwrap();
        assert!(partial.include_drop && partial.include_indexes);
        let d: DdlRequest = serde_json::from_value(json!({"scope": {"segments": []}})).unwrap();
        assert_eq!(d.options, DdlOptions::default());
    }

    #[test]
    fn schema_model_roundtrip() {
        let m = SchemaModel {
            scope: ObjectPath::root().child(ObjectKind::Database, "app"),
            tables: vec![TableModel {
                path: path("orders"),
                kind: ObjectKind::Table,
                schema: Some("public".into()),
                name: "orders".into(),
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
                    extra: None,
                }],
                primary_key: vec!["id".into()],
                unique_keys: vec![vec!["code".into()]],
                estimated_rows: Some(10),
            }],
            relations: vec![Relation {
                name: "fk".into(),
                from_table: path("orders"),
                from_columns: vec!["user_id".into()],
                to_table: path("users"),
                to_columns: vec!["id".into()],
                on_update: None,
                on_delete: Some("CASCADE".into()),
                from_optional: true,
                one_to_one: false,
                inferred: false,
            }],
            inferred: false,
            truncated: false,
        };
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["tables"][0]["primaryKey"], json!(["id"]));
        assert_eq!(json["tables"][0]["uniqueKeys"], json!([["code"]]));
        assert_eq!(json["tables"][0]["estimatedRows"], 10);
        assert_eq!(json["relations"][0]["fromColumns"], json!(["user_id"]));
        assert_eq!(json["relations"][0]["fromOptional"], true);
        assert_eq!(json["relations"][0]["oneToOne"], false);
        assert_eq!(serde_json::from_value::<SchemaModel>(json).unwrap(), m);
    }

    #[test]
    fn ddl_item_shape() {
        let item = DdlItem {
            path: path("t"),
            kind: ObjectKind::Table,
            section: DdlSection::PostData,
            sql: "CREATE INDEX i ON t (a);".into(),
            language: DdlLanguage::MongoShell,
            depends_on: vec![path("u")],
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["section"], "postData");
        assert_eq!(json["language"], "mongoShell");
        assert_eq!(json["dependsOn"].as_array().unwrap().len(), 1);
        assert_eq!(serde_json::from_value::<DdlItem>(json).unwrap(), item);
        assert_eq!(
            serde_json::to_value(DdlSection::PreData).unwrap(),
            "preData"
        );
        assert_eq!(serde_json::to_value(DdlLanguage::Sql).unwrap(), "sql");
    }

    #[test]
    fn stream_and_bulk_shapes() {
        let r: TableStreamRequest = serde_json::from_value(json!({
            "queryId": "00000000-0000-0000-0000-000000000000",
            "table": {"segments": []}
        }))
        .unwrap();
        assert!(r.columns.is_empty() && !r.order_by_key && r.batch_size.is_none());
        let b = BulkInsertRequest {
            table: path("t"),
            columns: vec!["a".into(), "b".into()],
            rows: vec![vec![
                EditValue::Text("1".into()),
                EditValue::Value(Value::Null),
            ]],
            on_conflict: OnConflict::Update,
            conflict_columns: vec![],
        };
        let json = serde_json::to_value(&b).unwrap();
        assert_eq!(json["onConflict"], "update");
        assert_eq!(json["rows"][0][0], json!({"kind": "text", "v": "1"}));
        assert_eq!(
            serde_json::from_value::<BulkInsertRequest>(json).unwrap(),
            b
        );
        assert_eq!(
            serde_json::to_value(BulkInsertResult {
                inserted: 1,
                updated: 2,
                skipped: 3
            })
            .unwrap(),
            json!({"inserted": 1, "updated": 2, "skipped": 3})
        );
    }
}
