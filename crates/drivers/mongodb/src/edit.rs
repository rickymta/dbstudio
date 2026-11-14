//! Document editing: preview (shell text) and apply (commands, transaction on replica sets).

use bson::{Bson, Document, doc};
use dbstudio_core::{
    ApplyResult, ChangeSet, DocInput, DocumentChange, DocumentChangeSet, Error, FieldEdit,
    FieldOriginal, Result,
};
use mongodb::Client;

use crate::conn::{Cmd, Conn, map_error, num};
use crate::convert::{doc_to_ejson, parse_ejson_doc, to_ejson, value_to_bson};
use crate::query::resolve;

fn bad(msg: impl Into<String>) -> Error {
    Error::Query {
        message: msg.into(),
        position: None,
    }
}

/// Shell literal of a value: `ObjectId("..")`, `ISODate("..")`, `NumberLong("..")`, else JSON.
pub fn shell_literal(b: &Bson) -> String {
    match b {
        Bson::ObjectId(o) => format!("ObjectId(\"{}\")", o.to_hex()),
        Bson::DateTime(dt) => match crate::convert::millis_to_iso(dt.timestamp_millis()) {
            Some(s) => format!("ISODate(\"{s}\")"),
            None => format!("new Date({})", dt.timestamp_millis()),
        },
        Bson::Int64(i) => format!("NumberLong(\"{i}\")"),
        Bson::Decimal128(d) => format!("NumberDecimal(\"{d}\")"),
        Bson::Document(d) => {
            let parts: Vec<String> = d
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}: {}",
                        to_ejson(&Bson::String(k.clone()), false, false),
                        shell_literal(v)
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Bson::Array(a) => format!(
            "[{}]",
            a.iter().map(shell_literal).collect::<Vec<_>>().join(", ")
        ),
        other => to_ejson(other, false, false),
    }
}

fn doc_literal(d: &Document) -> String {
    shell_literal(&Bson::Document(d.clone()))
}

fn input_doc(i: &DocInput) -> Result<Document> {
    match i {
        DocInput::Ejson { text } => parse_ejson_doc(text),
        DocInput::Value { value } => match value_to_bson(value)? {
            Bson::Document(d) => Ok(d),
            _ => Err(bad("a document is required")),
        },
    }
}

/// A field path is sent to `$set`/`$unset` as a dotted name, so a path element that itself
/// contains `.` or starts with `$` cannot be addressed (use Replace for such documents).
fn check_path(path: &[String]) -> Result<()> {
    if path.is_empty() || path.iter().any(String::is_empty) {
        return Err(bad("empty field path"));
    }
    if let Some(bad_name) = path.iter().find(|n| n.contains('.') || n.starts_with('$')) {
        return Err(bad(format!(
            "field '{bad_name}' contains '.' or starts with '$' and cannot be edited with $set/$unset; replace the document instead"
        )));
    }
    Ok(())
}

fn update_docs(set: &[FieldEdit], unset: &[Vec<String>]) -> Result<Document> {
    let mut u = Document::new();
    if !set.is_empty() {
        let mut s = Document::new();
        for f in set {
            check_path(&f.path)?;
            s.insert(f.path.join("."), value_to_bson(&f.value)?);
        }
        u.insert("$set", s);
    }
    if !unset.is_empty() {
        let mut s = Document::new();
        for p in unset {
            check_path(p)?;
            s.insert(p.join("."), "");
        }
        u.insert("$unset", s);
    }
    if u.is_empty() {
        return Err(bad("the update changes nothing"));
    }
    Ok(u)
}

/// Filter of a change: `{_id, <expected conditions>}`. A field that was present must still hold
/// its original value (wrapped in `$eq`, so a regex or a sub-document with `$` keys is compared
/// as a value, not read as an operator); a field that was absent must still be absent.
///
/// Limit: MongoDB equality on an array field also matches a document whose array merely
/// CONTAINS the original array as an element, and a path through an array uses the usual
/// membership semantics; for such fields the check is weaker than "unchanged".
fn guarded_filter(id: &dbstudio_core::Value, expected: &[FieldOriginal]) -> Result<Document> {
    let mut q = doc! {"_id": value_to_bson(id)?};
    for e in expected {
        check_path(&e.path)?;
        let path = e.path.join(".");
        let cond = match &e.value {
            None => doc! {"$exists": false},
            // `{f: null}` would also match a missing field.
            Some(v) => match value_to_bson(v)? {
                Bson::Null => doc! {"$type": "null"},
                Bson::Undefined => doc! {"$type": "undefined"},
                b => doc! {"$eq": b},
            },
        };
        q.insert(path, Bson::Document(cond));
    }
    Ok(q)
}

/// One planned write: the command, its shell text, and whether it must affect exactly one doc.
struct Step {
    index: u32,
    collection: String,
    /// `{_id}` of the target document and whether the filter also carries `expected` field
    /// conditions (optimistic concurrency check, see `explain_miss`).
    id_filter: Option<Document>,
    guarded: bool,
    command: Document,
    text: String,
    exactly_one: bool,
    is_insert: bool,
}

/// Order: deletes, updates (and replaces), inserts. `index` keeps the position in the request.
fn steps(set: &DocumentChangeSet) -> Result<Vec<Step>> {
    let (_, coll) = resolve(&set.collection).ok_or_else(|| bad("invalid collection path"))?;
    let (coll, _) = coll.ok_or_else(|| bad("invalid collection path"))?;
    let q = guarded_filter;
    let id_q = |id: &dbstudio_core::Value| -> Result<Option<Document>> {
        Ok(Some(doc! {"_id": value_to_bson(id)?}))
    };
    let mut deletes = vec![];
    let mut updates = vec![];
    let mut inserts = vec![];
    for (i, ch) in set.changes.iter().enumerate() {
        let index = i as u32;
        match ch {
            DocumentChange::Delete { id, expected } => {
                let query = q(id, expected)?;
                deletes.push(Step {
                    index,
                    collection: coll.clone(),
                    id_filter: id_q(id)?,
                    guarded: !expected.is_empty(),
                    text: format!(
                        "db.getCollection({}).deleteOne({})",
                        jstr(&coll),
                        doc_literal(&query)
                    ),
                    command: doc! {"delete": &coll, "deletes": [{"q": query, "limit": 1}]},
                    exactly_one: true,
                    is_insert: false,
                });
            }
            DocumentChange::Update {
                id,
                set: s,
                unset,
                expected,
            } => {
                let query = q(id, expected)?;
                let u = update_docs(s, unset)?;
                updates.push(Step {
                    index,
                    collection: coll.clone(),
                    id_filter: id_q(id)?,
                    guarded: !expected.is_empty(),
                    text: format!(
                        "db.getCollection({}).updateOne({}, {})",
                        jstr(&coll),
                        doc_literal(&query),
                        doc_literal(&u)
                    ),
                    command: doc! {"update": &coll, "updates": [{"q": query, "u": u}]},
                    exactly_one: true,
                    is_insert: false,
                });
            }
            DocumentChange::Replace {
                id,
                document,
                expected,
            } => {
                let query = q(id, expected)?;
                let d = input_doc(document)?;
                if d.keys().next().is_some_and(|k| k.starts_with('$')) {
                    return Err(bad(
                        "a replacement document cannot start with an operator ($...) field",
                    ));
                }
                updates.push(Step {
                    index,
                    collection: coll.clone(),
                    id_filter: id_q(id)?,
                    guarded: !expected.is_empty(),
                    text: format!(
                        "db.getCollection({}).replaceOne({}, {})",
                        jstr(&coll),
                        doc_literal(&query),
                        doc_literal(&d)
                    ),
                    command: doc! {"update": &coll, "updates": [{"q": query, "u": d}]},
                    exactly_one: true,
                    is_insert: false,
                });
            }
            DocumentChange::Insert { document } => {
                let d = input_doc(document)?;
                inserts.push(Step {
                    index,
                    collection: coll.clone(),
                    id_filter: None,
                    guarded: false,
                    text: format!(
                        "db.getCollection({}).insertOne({})",
                        jstr(&coll),
                        doc_literal(&d)
                    ),
                    command: doc! {"insert": &coll, "documents": [d]},
                    exactly_one: true,
                    is_insert: true,
                });
            }
        }
    }
    deletes.extend(updates);
    deletes.extend(inserts);
    Ok(deletes)
}

fn jstr(s: &str) -> String {
    to_ejson(&Bson::String(s.to_string()), false, false)
}

/// Statements `apply_changes` would run, in application order.
pub fn preview(changes: &ChangeSet) -> Result<Vec<String>> {
    match changes {
        ChangeSet::Documents(set) => Ok(steps(set)?.into_iter().map(|s| s.text).collect()),
        _ => Err(Error::Unsupported("data_editing".into())),
    }
}

async fn run_step(cmd: &mut Cmd<'_>, db: &str, step: &Step) -> Result<u64> {
    let reply = cmd.run(db, step.command.clone()).await?;
    let n = num(&reply, "n");
    if step.exactly_one && n != 1 && step.guarded {
        return Err(bad(explain_miss(cmd, db, step).await?));
    }
    if step.exactly_one && n != 1 {
        let what = if step.is_insert {
            "inserted"
        } else {
            "matched"
        };
        return Err(bad(format!("expected exactly 1 document {what}, got {n}")));
    }
    Ok(n.max(0) as u64)
}

/// Why a guarded write matched nothing: the document is gone, or a checked field changed.
async fn explain_miss(cmd: &mut Cmd<'_>, db: &str, step: &Step) -> Result<&'static str> {
    let Some(filter) = &step.id_filter else {
        return Ok("document was modified by another client");
    };
    let reply = cmd
        .run(
            db,
            doc! {
                "find": &step.collection, "filter": filter.clone(),
                "projection": {"_id": 1}, "limit": 1, "singleBatch": true,
            },
        )
        .await?;
    let found = reply
        .get_document("cursor")
        .ok()
        .and_then(|c| c.get_array("firstBatch").ok())
        .is_some_and(|b| !b.is_empty());
    Ok(if found {
        "document was modified by another client"
    } else {
        "document no longer exists"
    })
}

/// Apply a document change set.
pub async fn apply(conn: &Conn, changes: ChangeSet) -> Result<ApplyResult> {
    let ChangeSet::Documents(set) = changes else {
        return Err(Error::Unsupported("data_editing".into()));
    };
    if conn.read_only() {
        return Err(Error::ReadOnlyViolation {
            message: "the connection is read-only".into(),
            position: None,
        });
    }
    let (db, _) = resolve(&set.collection).ok_or_else(|| bad("invalid collection path"))?;
    let steps = steps(&set)?;
    apply_steps(&conn.client, &db, steps, conn.transactions).await
}

async fn apply_steps(
    client: &Client,
    db: &str,
    steps: Vec<Step>,
    transactions: bool,
) -> Result<ApplyResult> {
    let mut affected = Vec::with_capacity(steps.len());
    if transactions {
        let mut session = client.start_session().await.map_err(map_error)?;
        session.start_transaction().await.map_err(map_error)?;
        for step in &steps {
            let r = {
                let mut cmd = Cmd {
                    client,
                    session: Some(&mut session),
                };
                run_step(&mut cmd, db, step).await
            };
            match r {
                Ok(n) => affected.push(n),
                Err(e) => {
                    let _ = session.abort_transaction().await;
                    return Err(Error::ChangeFailed {
                        index: step.index,
                        message: e.message(),
                        applied: 0,
                    });
                }
            }
        }
        session.commit_transaction().await.map_err(map_error)?;
        Ok(ApplyResult {
            applied: steps.len() as u32,
            affected,
            atomic: true,
        })
    } else {
        let mut cmd = Cmd::pooled(client);
        for (applied, step) in steps.iter().enumerate() {
            match run_step(&mut cmd, db, step).await {
                Ok(n) => affected.push(n),
                Err(e) => {
                    return Err(Error::ChangeFailed {
                        index: step.index,
                        message: e.message(),
                        applied: applied as u32,
                    });
                }
            }
        }
        Ok(ApplyResult {
            applied: steps.len() as u32,
            affected,
            atomic: false,
        })
    }
}

/// Canonical EJSON of an inserted document, for messages.
#[allow(dead_code)]
pub fn describe(d: &Document) -> String {
    doc_to_ejson(d, false, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{ObjectKind, ObjectPath, Value};

    fn set(changes: Vec<DocumentChange>) -> ChangeSet {
        ChangeSet::Documents(DocumentChangeSet {
            collection: ObjectPath::root()
                .child(ObjectKind::Database, "app")
                .child(ObjectKind::Folder, "collections")
                .child(ObjectKind::Collection, "users"),
            changes,
        })
    }

    #[test]
    fn preview_orders_delete_update_insert() {
        let id = Value::Ext(dbstudio_core::ExtValue::new(
            "objectId",
            "65f0a1b2c3d4e5f607182930",
        ));
        let p = preview(&set(vec![
            DocumentChange::Insert {
                document: DocInput::Ejson {
                    text: "{\"name\":\"x\"}".into(),
                },
            },
            DocumentChange::Update {
                id: id.clone(),
                set: vec![FieldEdit {
                    path: vec!["address".into(), "city".into()],
                    value: Value::Text("Hue".into()),
                }],
                unset: vec![vec!["old".into()]],
                expected: vec![],
            },
            DocumentChange::Delete {
                id,
                expected: vec![],
            },
        ]))
        .unwrap();
        assert!(
            p[0].contains("deleteOne({\"_id\": ObjectId(\"65f0a1b2c3d4e5f607182930\")})"),
            "{p:?}"
        );
        assert!(
            p[1].contains("updateOne")
                && p[1].contains("$set")
                && p[1].contains("\"address.city\": \"Hue\"")
                && p[1].contains("$unset")
        );
        assert!(p[2].contains("insertOne({\"name\": \"x\"})"));
    }

    #[test]
    fn unsafe_paths_and_operator_replacements_are_rejected() {
        let upd = |path: &[&str]| {
            preview(&set(vec![DocumentChange::Update {
                id: Value::Int(1),
                set: vec![FieldEdit {
                    path: path.iter().map(|s| s.to_string()).collect(),
                    value: Value::Int(1),
                }],
                unset: vec![],
                expected: vec![],
            }]))
        };
        assert!(upd(&["a", "b"]).is_ok());
        assert!(upd(&["a", "0", "b"]).is_ok());
        assert!(
            upd(&["a.b"]).is_err(),
            "dotted key would address a nested path"
        );
        assert!(upd(&["a", "$x"]).is_err());
        assert!(upd(&[]).is_err());
        let unset = preview(&set(vec![DocumentChange::Update {
            id: Value::Int(1),
            set: vec![],
            unset: vec![vec!["x.y".into()]],
            expected: vec![],
        }]));
        assert!(unset.is_err());
        let rep = |text: &str| {
            preview(&set(vec![DocumentChange::Replace {
                id: Value::Int(1),
                document: DocInput::Ejson { text: text.into() },
                expected: vec![],
            }]))
        };
        assert!(rep("{\"a\": 1}").is_ok());
        assert!(rep("{\"$set\": {\"a\": 1}}").is_err());
    }

    #[test]
    fn expected_fields_become_filter_conditions() {
        let p = preview(&set(vec![
            DocumentChange::Update {
                id: Value::Int(1),
                set: vec![FieldEdit {
                    path: vec!["a".into(), "b".into()],
                    value: Value::Int(2),
                }],
                unset: vec![],
                expected: vec![
                    FieldOriginal {
                        path: vec!["a".into(), "b".into()],
                        value: Some(Value::Int(1)),
                    },
                    FieldOriginal {
                        path: vec!["c".into()],
                        value: None,
                    },
                    FieldOriginal {
                        path: vec!["n".into()],
                        value: Some(Value::Null),
                    },
                ],
            },
            DocumentChange::Delete {
                id: Value::Int(1),
                expected: vec![],
            },
        ]))
        .unwrap();
        // Deletes run first; the update is second.
        assert_eq!(p[0], "db.getCollection(\"users\").deleteOne({\"_id\": 1})");
        assert!(
            p[1].contains("{\"_id\": 1, \"a.b\": {\"$eq\": 1}, \"c\": {\"$exists\": false}, \"n\": {\"$type\": \"null\"}}"),
            "{p:?}"
        );
    }

    #[test]
    fn invalid_input_is_rejected() {
        let e = preview(&set(vec![DocumentChange::Insert {
            document: DocInput::Ejson {
                text: "{bad".into(),
            },
        }]))
        .unwrap_err();
        assert_eq!(e.kind(), "query");
        let e = preview(&set(vec![DocumentChange::Update {
            id: Value::Int(1),
            set: vec![],
            unset: vec![],
            expected: vec![],
        }]))
        .unwrap_err();
        assert_eq!(e.kind(), "query");
    }
}
