//! Schema inference by sampling, schema model (ERD), structure and DDL text.

use std::collections::BTreeMap;

use bson::{Bson, Document, doc};
use dbstudio_core::{
    ColumnDetail, DdlItem, DdlLanguage, DdlSection, ObjectKind, ObjectPath, Relation, Result,
    ValueKind,
};
use mongodb::Client;

use crate::conn::{Cmd, num};
use crate::exec::pending_from_reply;

/// Short type name used in column metadata.
pub fn type_label(b: &Bson) -> &'static str {
    match b {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => "javascript",
        Bson::Int32(_) => "int32",
        Bson::Int64(_) => "int64",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal",
        Bson::Undefined => "undefined",
        Bson::MaxKey => "maxKey",
        Bson::MinKey => "minKey",
        Bson::DbPointer(_) => "dbPointer",
    }
}

fn kind_of(label: &str) -> ValueKind {
    match label {
        "double" => ValueKind::Float,
        "string" => ValueKind::Text,
        "array" => ValueKind::Array,
        "object" => ValueKind::Document,
        "bool" => ValueKind::Bool,
        "int32" | "int64" => ValueKind::Int,
        "date" => ValueKind::Timestamp,
        "decimal" => ValueKind::Decimal,
        "binData" => ValueKind::Bytes,
        "null" => ValueKind::Null,
        _ => ValueKind::Ext,
    }
}

/// Columns inferred from a sample: top-level keys in first-seen order (`_id` first), the
/// union of the types seen (`string|int32`), `seen N%` in `extra`.
pub fn infer_columns(docs: &[Document]) -> Vec<ColumnDetail> {
    #[derive(Default)]
    struct Stat {
        types: Vec<&'static str>,
        seen: usize,
        has_null: bool,
    }
    let mut order: Vec<String> = Vec::new();
    let mut stats: BTreeMap<String, Stat> = BTreeMap::new();
    for d in docs {
        for (k, v) in d {
            let s = stats.entry(k.clone()).or_insert_with(|| {
                order.push(k.clone());
                Stat::default()
            });
            s.seen += 1;
            let t = type_label(v);
            if t == "null" {
                s.has_null = true;
            }
            if !s.types.contains(&t) {
                s.types.push(t);
            }
        }
    }
    if let Some(i) = order.iter().position(|k| k == "_id") {
        let id = order.remove(i);
        order.insert(0, id);
    }
    let total = docs.len().max(1);
    order
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let s = &stats[k];
            let non_null: Vec<_> = s.types.iter().copied().filter(|t| *t != "null").collect();
            let kind = match non_null.as_slice() {
                [one] => kind_of(one),
                [] => ValueKind::Null,
                many if many.iter().all(|t| matches!(*t, "int32" | "int64")) => ValueKind::Int,
                _ => ValueKind::Ext,
            };
            let pct = (s.seen * 100 + total / 2) / total;
            ColumnDetail {
                name: k.clone(),
                ordinal: (i + 1) as u32,
                type_name: if s.types.is_empty() {
                    "unknown".into()
                } else {
                    s.types.join("|")
                },
                value_kind: kind,
                nullable: k != "_id" && (s.seen < docs.len() || s.has_null),
                default: None,
                comment: None,
                primary_key: k == "_id",
                extra: Some(format!("seen {pct}%")),
            }
        })
        .collect()
}

/// `$sample` of `size` documents (or the first `size` when the collection is small).
pub async fn sample(cmd: &mut Cmd<'_>, db: &str, coll: &str, size: u32) -> Result<Vec<Document>> {
    let r = cmd
        .run(
            db,
            doc! {"aggregate": coll, "pipeline": [{"$sample": {"size": size as i64}}],
            "cursor": {"batchSize": size as i64}},
        )
        .await?;
    let mut p = pending_from_reply(&r, db)?;
    let mut docs: Vec<Document> = p.buffer.drain(..).collect();
    // Kill leftover cursor when the batch was smaller than requested.
    if p.id != 0 {
        crate::exec::kill_cursor(cmd, &p).await;
    }
    docs.truncate(size as usize);
    Ok(docs)
}

/// Relations inferred from `fooId` / `foo_id` ObjectId fields to a collection `foo` / `foos`.
pub fn infer_relations(tables: &[(ObjectPath, String, Vec<ColumnDetail>)]) -> Vec<Relation> {
    let mut rels = Vec::new();
    for (path, name, cols) in tables {
        for c in cols {
            if c.name == "_id" || !c.type_name.contains("objectId") {
                continue;
            }
            let lower = c.name.to_ascii_lowercase();
            let stem = if let Some(s) = lower.strip_suffix("_id") {
                s.to_string()
            } else if c.name.ends_with("Id") && c.name.len() > 2 {
                lower[..lower.len() - 2].to_string()
            } else {
                continue;
            };
            let candidates = [
                stem.clone(),
                format!("{stem}s"),
                format!("{stem}es"),
                format!("{}ies", stem.trim_end_matches('y')),
            ];
            let target = tables.iter().find(|(_, n, _)| {
                n != name && candidates.iter().any(|c| c.eq_ignore_ascii_case(n))
            });
            if let Some((to_path, to_name, _)) = target {
                rels.push(Relation {
                    name: format!("{name}.{} -> {to_name}._id", c.name),
                    from_table: path.clone(),
                    from_columns: vec![c.name.clone()],
                    to_table: to_path.clone(),
                    to_columns: vec!["_id".into()],
                    on_update: None,
                    on_delete: None,
                    from_optional: c.nullable,
                    one_to_one: false,
                    inferred: true,
                });
            }
        }
    }
    rels
}

/// `listCollections` entries of a database: (name, type, options).
pub async fn list_collections(
    cmd: &mut Cmd<'_>,
    db: &str,
) -> Result<Vec<(String, String, Document)>> {
    let r = cmd.run(db, doc! {"listCollections": 1}).await?;
    let mut p = pending_from_reply(&r, db)?;
    let mut out: Vec<(String, String, Document)> = p
        .buffer
        .drain(..)
        .filter_map(|d| {
            Some((
                d.get_str("name").ok()?.to_string(),
                d.get_str("type").unwrap_or("collection").to_string(),
                d.get_document("options").cloned().unwrap_or_default(),
            ))
        })
        .collect();
    while p.id != 0 {
        let more = cmd
            .run(
                db,
                doc! {"getMore": p.id, "collection": "$cmd.listCollections"},
            )
            .await?;
        let c = more.get_document("cursor").cloned().unwrap_or_default();
        p.id = num(&c, "id");
        for b in c.get_array("nextBatch").cloned().unwrap_or_default() {
            if let Bson::Document(d) = b {
                if let Ok(n) = d.get_str("name") {
                    out.push((
                        n.to_string(),
                        d.get_str("type").unwrap_or("collection").to_string(),
                        d.get_document("options").cloned().unwrap_or_default(),
                    ));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Index specifications of a collection.
pub async fn list_indexes(cmd: &mut Cmd<'_>, db: &str, coll: &str) -> Result<Vec<Document>> {
    let r = match cmd.run(db, doc! {"listIndexes": coll}).await {
        Ok(r) => r,
        // Namespace not found (e.g. a view): no indexes.
        Err(dbstudio_core::Error::Query { message, .. }) if message.contains("code 26") => {
            return Ok(vec![]);
        }
        Err(e) => return Err(e),
    };
    let p = pending_from_reply(&r, db)?;
    Ok(p.buffer.into_iter().collect())
}

/// Index key columns of a spec.
pub fn index_columns(spec: &Document) -> Vec<String> {
    spec.get_document("key")
        .map(|k| k.keys().cloned().collect())
        .unwrap_or_default()
}

fn shell_str(s: &str) -> String {
    crate::convert::to_ejson(&Bson::String(s.to_string()), false, false)
}

/// `db.createCollection(...)` and `db.coll.createIndex(...)` for one collection or view.
pub async fn ddl_for(
    client: &Client,
    db: &str,
    name: &str,
    kind: &str,
    options: &Document,
    ddl: &dbstudio_core::DdlOptions,
) -> Result<Vec<(ObjectKind, DdlSection, String)>> {
    let (include_indexes, include_drop) = (ddl.include_indexes, ddl.include_drop);
    let mut out = Vec::new();
    let mut text = String::new();
    if include_drop {
        text.push_str(&format!("db.getCollection({}).drop();\n", shell_str(name)));
    }
    if kind == "view" {
        let on = options.get_str("viewOn").unwrap_or("");
        let pipeline = options.get_array("pipeline").cloned().unwrap_or_default();
        text.push_str(&format!(
            "db.createView({}, {}, {});",
            shell_str(name),
            shell_str(on),
            crate::convert::to_ejson(&Bson::Array(pipeline), false, false)
        ));
        out.push((ObjectKind::View, DdlSection::PreData, text));
        return Ok(out);
    }
    if options.is_empty() {
        text.push_str(&format!("db.createCollection({});", shell_str(name)));
    } else {
        text.push_str(&format!(
            "db.createCollection({}, {});",
            shell_str(name),
            crate::convert::doc_to_ejson(options, false, false)
        ));
    }
    out.push((ObjectKind::Collection, DdlSection::PreData, text));
    if include_indexes {
        let mut cmd = Cmd::pooled(client);
        for spec in list_indexes(&mut cmd, db, name).await? {
            let iname = spec.get_str("name").unwrap_or("");
            if iname == "_id_" {
                continue;
            }
            let key = spec.get_document("key").cloned().unwrap_or_default();
            let mut opts = Document::new();
            for (k, v) in &spec {
                if !matches!(k.as_str(), "v" | "key" | "ns") {
                    opts.insert(k.clone(), v.clone());
                }
            }
            out.push((
                ObjectKind::Index,
                DdlSection::PostData,
                format!(
                    "db.getCollection({}).createIndex({}, {});",
                    shell_str(name),
                    crate::convert::doc_to_ejson(&key, false, false),
                    crate::convert::doc_to_ejson(&opts, false, false)
                ),
            ));
        }
    }
    Ok(out)
}

/// Build a [`DdlItem`].
pub fn ddl_item(
    db: &str,
    name: &str,
    kind: ObjectKind,
    section: DdlSection,
    text: String,
) -> DdlItem {
    let folder = if kind == ObjectKind::View {
        dbstudio_core::folder::VIEWS
    } else {
        dbstudio_core::folder::COLLECTIONS
    };
    let coll_path = ObjectPath::root()
        .child(ObjectKind::Database, db)
        .child(ObjectKind::Folder, folder)
        .child(
            if kind == ObjectKind::View {
                ObjectKind::View
            } else {
                ObjectKind::Collection
            },
            name,
        );
    DdlItem {
        path: coll_path.clone(),
        kind,
        section,
        sql: text,
        language: DdlLanguage::MongoShell,
        depends_on: if section == DdlSection::PostData {
            vec![coll_path]
        } else {
            vec![]
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    #[test]
    fn columns_union_with_percentages() {
        let docs = vec![
            doc! {"_id": 1, "name": "a", "age": 3},
            doc! {"_id": 2, "name": "b", "age": "x", "extra": Bson::Null},
            doc! {"_id": 3, "name": "c"},
            doc! {"_id": 4, "name": "d"},
        ];
        let cols = infer_columns(&docs);
        let names: Vec<_> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["_id", "name", "age", "extra"]);
        assert!(cols[0].primary_key && !cols[0].nullable);
        assert_eq!(cols[1].type_name, "string");
        assert_eq!(cols[1].extra.as_deref(), Some("seen 100%"));
        assert_eq!(cols[2].type_name, "int32|string");
        assert!(cols[2].nullable);
        assert_eq!(cols[2].extra.as_deref(), Some("seen 50%"));
    }

    #[test]
    fn relations_are_inferred() {
        let oid = |n: &str| ColumnDetail {
            name: n.into(),
            ordinal: 1,
            type_name: "objectId".into(),
            value_kind: ValueKind::Ext,
            nullable: false,
            default: None,
            comment: None,
            primary_key: false,
            extra: None,
        };
        let p = |n: &str| ObjectPath::root().child(ObjectKind::Collection, n);
        let tables = vec![
            (
                p("orders"),
                "orders".to_string(),
                vec![oid("userId"), oid("product_id")],
            ),
            (p("users"), "users".to_string(), vec![]),
            (p("products"), "products".to_string(), vec![]),
        ];
        let rels = infer_relations(&tables);
        assert_eq!(rels.len(), 2);
        assert!(rels.iter().all(|r| r.inferred && r.to_columns == ["_id"]));
        assert!(rels.iter().any(|r| r.to_table == p("users")));
        assert!(rels.iter().any(|r| r.to_table == p("products")));
    }
}
