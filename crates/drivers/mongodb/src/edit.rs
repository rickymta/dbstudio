//! Document editing: preview (shell text) and apply (commands, transaction on replica sets).

use bson::{Bson, Document, doc};
use dbstudio_core::{
    ApplyResult, ChangeSet, DocInput, DocumentChange, DocumentChangeSet, Error, FieldEdit, Result,
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

fn update_docs(set: &[FieldEdit], unset: &[Vec<String>]) -> Result<Document> {
    let mut u = Document::new();
    if !set.is_empty() {
        let mut s = Document::new();
        for f in set {
            if f.path.is_empty() {
                return Err(bad("empty field path"));
            }
            s.insert(f.path.join("."), value_to_bson(&f.value)?);
        }
        u.insert("$set", s);
    }
    if !unset.is_empty() {
        let mut s = Document::new();
        for p in unset {
            s.insert(p.join("."), "");
        }
        u.insert("$unset", s);
    }
    if u.is_empty() {
        return Err(bad("the update changes nothing"));
    }
    Ok(u)
}

/// One planned write: the command, its shell text, and whether it must affect exactly one doc.
struct Step {
    index: u32,
    command: Document,
    text: String,
    exactly_one: bool,
    is_insert: bool,
}

/// Order: deletes, updates (and replaces), inserts. `index` keeps the position in the request.
fn steps(set: &DocumentChangeSet) -> Result<Vec<Step>> {
    let (_, coll) = resolve(&set.collection).ok_or_else(|| bad("invalid collection path"))?;
    let (coll, _) = coll.ok_or_else(|| bad("invalid collection path"))?;
    let q =
        |id: &dbstudio_core::Value| -> Result<Document> { Ok(doc! {"_id": value_to_bson(id)?}) };
    let mut deletes = vec![];
    let mut updates = vec![];
    let mut inserts = vec![];
    for (i, ch) in set.changes.iter().enumerate() {
        let index = i as u32;
        match ch {
            DocumentChange::Delete { id } => {
                let query = q(id)?;
                deletes.push(Step {
                    index,
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
            DocumentChange::Update { id, set: s, unset } => {
                let query = q(id)?;
                let u = update_docs(s, unset)?;
                updates.push(Step {
                    index,
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
            DocumentChange::Replace { id, document } => {
                let query = q(id)?;
                let d = input_doc(document)?;
                updates.push(Step {
                    index,
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
            },
            DocumentChange::Delete { id },
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
        }]))
        .unwrap_err();
        assert_eq!(e.kind(), "query");
    }
}
