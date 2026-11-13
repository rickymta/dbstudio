//! Data editing: editability, change sets and their results.

use serde::{Deserialize, Serialize};

use crate::{ObjectPath, Value};

/// Whether (and how) a table's rows can be edited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Editability {
    /// Rows can be edited.
    pub editable: bool,
    /// PK, else first unique NOT NULL index; Mongo `["_id"]`.
    pub key_columns: Vec<String>,
    /// Identity/generated/computed/rowversion columns.
    pub read_only_columns: Vec<String>,
    /// Why not editable.
    pub reason: Option<NotEditableReason>,
    /// `apply_changes` is all-or-nothing (false: Mongo standalone, Redis).
    pub atomic: bool,
}

/// Why a result or table is not editable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NotEditableReason {
    /// No primary key or unique NOT NULL index.
    NoUniqueKey,
    /// A view or other non-table object.
    NotATable,
    /// The connection is read-only.
    ReadOnlyConnection,
    /// The driver cannot edit this object.
    Unsupported,
}

/// A set of edits of one model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "model", rename_all = "camelCase")]
pub enum ChangeSet {
    /// Relational rows.
    Rows(RowChangeSet),
    /// Documents.
    Documents(DocumentChangeSet),
    /// Redis keys.
    Keys(KeyChangeSet),
}

/// Edits of table rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RowChangeSet {
    /// Table edited.
    pub table: ObjectPath,
    /// Key columns used to address rows.
    pub key_columns: Vec<String>,
    /// Changes.
    pub changes: Vec<RowChange>,
}

/// One row edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum RowChange {
    /// Update; `key` is aligned with `key_columns`, values as fetched.
    Update {
        /// Key of the row.
        key: Vec<Value>,
        /// New cell values.
        set: Vec<CellEdit>,
    },
    /// Insert a row.
    Insert {
        /// Cell values.
        values: Vec<CellEdit>,
    },
    /// Delete a row.
    Delete {
        /// Key of the row.
        key: Vec<Value>,
    },
}

/// One cell assignment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CellEdit {
    /// Column name.
    pub column: String,
    /// New value.
    pub value: EditValue,
}

/// A value being written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "v", rename_all = "camelCase")]
pub enum EditValue {
    /// Exact typed value (NULL = `Value::Null`, bytes from file...).
    Value(Value),
    /// User-typed text; rendered as a string literal, the database coerces it to the column type.
    Text(String),
    /// The `DEFAULT` keyword.
    Default,
}

/// Edits of documents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentChangeSet {
    /// Collection edited.
    pub collection: ObjectPath,
    /// Changes.
    pub changes: Vec<DocumentChange>,
}

/// One document edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum DocumentChange {
    /// Insert.
    Insert {
        /// The document.
        document: DocInput,
    },
    /// Replace the document with this `_id`.
    Replace {
        /// `_id` value.
        id: Value,
        /// New document.
        document: DocInput,
    },
    /// `$set` / `$unset` by path.
    Update {
        /// `_id` value.
        id: Value,
        /// Fields to set.
        set: Vec<FieldEdit>,
        /// Paths to unset.
        #[serde(default)]
        unset: Vec<Vec<String>>,
    },
    /// Delete.
    Delete {
        /// `_id` value.
        id: Value,
    },
}

/// A document typed by the user or built by the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DocInput {
    /// Typed value.
    Value {
        /// The document.
        value: Value,
    },
    /// Relaxed/canonical Extended JSON typed by the user.
    Ejson {
        /// EJSON text.
        text: String,
    },
}

/// One field assignment of a document update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldEdit {
    /// Field path, e.g. `["address", "city"]`.
    pub path: Vec<String>,
    /// New value.
    pub value: Value,
}

/// Edits of Redis keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyChangeSet {
    /// Database node.
    pub database: ObjectPath,
    /// Changes.
    pub changes: Vec<KeyChange>,
}

/// One key edit. `key` uses the Key segment name encoding
/// ([`crate::keyvalue::encode_key_name`]); member/field/value: `Text` or `Bytes`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum KeyChange {
    /// `SET` (optionally with TTL).
    #[serde(rename_all = "camelCase")]
    SetString {
        /// Key.
        key: String,
        /// Value.
        value: Value,
        /// Expiry in milliseconds.
        #[serde(default)]
        ttl_ms: Option<u64>,
    },
    /// `HSET`.
    HashSet {
        /// Key.
        key: String,
        /// `(field, value)` pairs.
        entries: Vec<(Value, Value)>,
    },
    /// `HDEL`.
    HashDelete {
        /// Key.
        key: String,
        /// Fields.
        fields: Vec<Value>,
    },
    /// `LSET`.
    ListSet {
        /// Key.
        key: String,
        /// Index.
        index: i64,
        /// Value.
        value: Value,
    },
    /// `LPUSH`/`RPUSH`.
    ListPush {
        /// Key.
        key: String,
        /// Values.
        values: Vec<Value>,
        /// Push at the head.
        head: bool,
    },
    /// Remove by index: LSET to a random sentinel + LREM 1 sentinel (inside MULTI).
    ListRemoveAt {
        /// Key.
        key: String,
        /// Index.
        index: i64,
    },
    /// `SADD`.
    SetAdd {
        /// Key.
        key: String,
        /// Members.
        members: Vec<Value>,
    },
    /// `SREM`.
    SetRemove {
        /// Key.
        key: String,
        /// Members.
        members: Vec<Value>,
    },
    /// `ZADD`.
    ZAdd {
        /// Key.
        key: String,
        /// `(member, score text)`.
        members: Vec<(Value, String)>,
    },
    /// `ZREM`.
    ZRemove {
        /// Key.
        key: String,
        /// Members.
        members: Vec<Value>,
    },
    /// `XADD`.
    StreamAdd {
        /// Key.
        key: String,
        /// Entry id; `None` = `*`.
        id: Option<String>,
        /// `(field, value)` pairs.
        fields: Vec<(Value, Value)>,
    },
    /// `XDEL`.
    StreamDelete {
        /// Key.
        key: String,
        /// Entry ids.
        ids: Vec<String>,
    },
    /// `PEXPIRE` / `PERSIST`.
    #[serde(rename_all = "camelCase")]
    Expire {
        /// Key.
        key: String,
        /// `None` = `PERSIST`.
        ttl_ms: Option<u64>,
    },
    /// `RENAMENX`; an existing target => `ChangeFailed`.
    #[serde(rename_all = "camelCase")]
    Rename {
        /// Key.
        key: String,
        /// New name.
        new_key: String,
    },
    /// `DEL`.
    Delete {
        /// Keys.
        keys: Vec<String>,
    },
}

/// Outcome of `apply_changes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyResult {
    /// Changes applied.
    pub applied: u32,
    /// Affected rows/documents per change.
    pub affected: Vec<u64>,
    /// Whether the whole set was applied atomically.
    pub atomic: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rt<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(
        v: &T,
    ) -> serde_json::Value {
        let json = serde_json::to_value(v).unwrap();
        let back: T = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(&back, v, "round trip of {json}");
        json
    }

    fn table() -> ObjectPath {
        ObjectPath::root().child(crate::ObjectKind::Table, "t")
    }

    #[test]
    fn rows_change_set_shape() {
        let cs = ChangeSet::Rows(RowChangeSet {
            table: table(),
            key_columns: vec!["id".into()],
            changes: vec![
                RowChange::Update {
                    key: vec![Value::Int(1)],
                    set: vec![CellEdit {
                        column: "n".into(),
                        value: EditValue::Text("x".into()),
                    }],
                },
                RowChange::Insert {
                    values: vec![
                        CellEdit {
                            column: "a".into(),
                            value: EditValue::Default,
                        },
                        CellEdit {
                            column: "b".into(),
                            value: EditValue::Value(Value::Null),
                        },
                    ],
                },
                RowChange::Delete {
                    key: vec![Value::Int(2)],
                },
            ],
        });
        let json = rt(&cs);
        assert_eq!(json["model"], "rows");
        assert_eq!(json["keyColumns"], json!(["id"]));
        assert_eq!(
            json["changes"][0],
            json!({"op": "update", "key": [{"t": "int", "v": 1}],
                   "set": [{"column": "n", "value": {"kind": "text", "v": "x"}}]})
        );
        assert_eq!(
            json["changes"][1]["values"][0]["value"],
            json!({"kind": "default"})
        );
        assert_eq!(
            json["changes"][1]["values"][1]["value"],
            json!({"kind": "value", "v": {"t": "null"}})
        );
    }

    #[test]
    fn documents_change_set_shape() {
        let cs = ChangeSet::Documents(DocumentChangeSet {
            collection: table(),
            changes: vec![
                DocumentChange::Insert {
                    document: DocInput::Ejson {
                        text: "{\"a\":1}".into(),
                    },
                },
                DocumentChange::Update {
                    id: Value::Ext(crate::ExtValue::new("objectId", "65f0a1b2c3d4e5f6a7b8c9d0")),
                    set: vec![FieldEdit {
                        path: vec!["address".into(), "city".into()],
                        value: Value::Text("Hue".into()),
                    }],
                    unset: vec![vec!["old".into()]],
                },
                DocumentChange::Replace {
                    id: Value::Int(1),
                    document: DocInput::Value {
                        value: Value::Document(vec![("a".into(), Value::Int(1))]),
                    },
                },
                DocumentChange::Delete { id: Value::Int(1) },
            ],
        });
        let json = rt(&cs);
        assert_eq!(json["model"], "documents");
        assert_eq!(
            json["changes"][0]["document"],
            json!({"kind": "ejson", "text": "{\"a\":1}"})
        );
        assert_eq!(json["changes"][1]["unset"], json!([["old"]]));
        // unset is optional on input.
        let upd: DocumentChange =
            serde_json::from_value(json!({"op": "update", "id": {"t": "int", "v": 1}, "set": []}))
                .unwrap();
        assert!(matches!(upd, DocumentChange::Update { unset, .. } if unset.is_empty()));
    }

    #[test]
    fn key_changes_use_camel_case_fields() {
        let cs = ChangeSet::Keys(KeyChangeSet {
            database: ObjectPath::root().child(crate::ObjectKind::Database, "0"),
            changes: vec![
                KeyChange::SetString {
                    key: "k".into(),
                    value: Value::Text("v".into()),
                    ttl_ms: Some(1500),
                },
                KeyChange::Expire {
                    key: "k".into(),
                    ttl_ms: None,
                },
                KeyChange::Rename {
                    key: "a".into(),
                    new_key: "b".into(),
                },
                KeyChange::ZAdd {
                    key: "z".into(),
                    members: vec![(Value::Text("m".into()), "1.5".into())],
                },
                KeyChange::StreamAdd {
                    key: "s".into(),
                    id: None,
                    fields: vec![(Value::Text("f".into()), Value::Text("v".into()))],
                },
                KeyChange::ListPush {
                    key: "l".into(),
                    values: vec![Value::Text("x".into())],
                    head: true,
                },
                KeyChange::Delete {
                    keys: vec!["a".into(), "b".into()],
                },
                KeyChange::HashSet {
                    key: "h".into(),
                    entries: vec![(Value::Text("f".into()), Value::Text("v".into()))],
                },
                KeyChange::HashDelete {
                    key: "h".into(),
                    fields: vec![],
                },
                KeyChange::ListSet {
                    key: "l".into(),
                    index: -1,
                    value: Value::Null,
                },
                KeyChange::ListRemoveAt {
                    key: "l".into(),
                    index: 0,
                },
                KeyChange::SetAdd {
                    key: "s".into(),
                    members: vec![],
                },
                KeyChange::SetRemove {
                    key: "s".into(),
                    members: vec![],
                },
                KeyChange::ZRemove {
                    key: "z".into(),
                    members: vec![],
                },
                KeyChange::StreamDelete {
                    key: "s".into(),
                    ids: vec!["1-0".into()],
                },
            ],
        });
        let json = rt(&cs);
        assert_eq!(json["model"], "keys");
        let c = &json["changes"];
        assert_eq!(
            c[0],
            json!({"op": "setString", "key": "k", "value": {"t": "text", "v": "v"}, "ttlMs": 1500})
        );
        assert_eq!(c[1], json!({"op": "expire", "key": "k", "ttlMs": null}));
        assert_eq!(c[2], json!({"op": "rename", "key": "a", "newKey": "b"}));
        assert_eq!(c[3]["op"], "zAdd");
        assert_eq!(c[4]["id"], json!(null));
        assert_eq!(c[5]["head"], true);
        assert_eq!(c[6], json!({"op": "delete", "keys": ["a", "b"]}));
        assert_eq!(c[10]["op"], "listRemoveAt");
        assert_eq!(c[14]["op"], "streamDelete");
        // ttlMs is optional on input.
        let k: KeyChange =
            serde_json::from_value(json!({"op": "setString", "key": "k", "value": {"t": "null"}}))
                .unwrap();
        assert!(matches!(k, KeyChange::SetString { ttl_ms: None, .. }));
    }

    #[test]
    fn editability_and_apply_result_shapes() {
        let e = Editability {
            editable: false,
            key_columns: vec![],
            read_only_columns: vec!["rv".into()],
            reason: Some(NotEditableReason::NoUniqueKey),
            atomic: true,
        };
        let json = rt(&e);
        assert_eq!(json["readOnlyColumns"], json!(["rv"]));
        assert_eq!(json["reason"], "noUniqueKey");
        for (r, s) in [
            (NotEditableReason::NotATable, "notATable"),
            (NotEditableReason::ReadOnlyConnection, "readOnlyConnection"),
            (NotEditableReason::Unsupported, "unsupported"),
        ] {
            assert_eq!(serde_json::to_value(r).unwrap(), s);
        }
        assert_eq!(
            rt(&ApplyResult {
                applied: 2,
                affected: vec![1, 1],
                atomic: false
            }),
            json!({"applied": 2, "affected": [1, 1], "atomic": false})
        );
    }
}
