//! Statement execution: shell statements are turned into raw database commands so that the
//! same code runs on the pool and inside a lease session (transactions).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use bson::{Bson, Document, doc};
use dbstudio_core::{
    ColumnMeta, EndOutcome, Error, Result, ResultSink, StatementEnd, StatementStart, Value,
    ValueKind,
};
use dbstudio_sql::mongo_shell::{ShellKind, ShellStatement, ShellValue};
use uuid::Uuid;

use crate::conn::{Cmd, num};
use crate::convert::doc_to_value;
use crate::shell::{shell_to_bson, shell_to_doc};

/// Default rows per streamed batch.
pub const DEFAULT_BATCH: usize = 1000;

/// Name of the single column of document results.
pub const DOC_COLUMN: &str = "$document";

fn bad(msg: impl Into<String>) -> Error {
    Error::Query {
        message: msg.into(),
        position: None,
    }
}

/// Per-request settings.
#[derive(Clone)]
pub struct RunCtx {
    pub query_id: Uuid,
    pub max_rows: Option<u64>,
    pub batch: usize,
    pub cancel: Arc<AtomicBool>,
    /// Defence in depth for read-only profiles (the command layer already blocks writes):
    /// refuse every statement that is not a read.
    pub read_only: bool,
}

impl RunCtx {
    fn check_cancel(&self) -> Result<()> {
        if self.cancel.load(Ordering::Acquire) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// A server-side cursor that still has documents (leases keep it for `fetch_more`).
#[derive(Debug)]
pub struct Pending {
    pub db: String,
    pub coll: String,
    pub id: i64,
    pub buffer: VecDeque<Document>,
}

pub fn doc_column() -> ColumnMeta {
    ColumnMeta {
        name: DOC_COLUMN.into(),
        type_name: "document".into(),
        value_kind: ValueKind::Document,
        nullable: Some(false),
    }
}

fn split_ns(ns: &str, db: &str) -> (String, String) {
    match ns.split_once('.') {
        Some((d, c)) => (d.to_string(), c.to_string()),
        None => (db.to_string(), ns.to_string()),
    }
}

/// Read the `cursor` sub-document of a command reply.
pub fn pending_from_reply(reply: &Document, db: &str) -> Result<Pending> {
    let c = reply
        .get_document("cursor")
        .map_err(|_| bad("the server did not return a cursor"))?;
    let (d, coll) = split_ns(c.get_str("ns").unwrap_or(""), db);
    let first = c
        .get_array("firstBatch")
        .map(|a| a.iter().filter_map(|b| b.as_document().cloned()).collect())
        .unwrap_or_default();
    Ok(Pending {
        db: d,
        coll,
        id: num(c, "id"),
        buffer: first,
    })
}

async fn fetch_next(cmd: &mut Cmd<'_>, p: &mut Pending, batch: usize) -> Result<()> {
    let reply = cmd
        .run(
            &p.db,
            doc! {"getMore": p.id, "collection": &p.coll, "batchSize": batch as i64},
        )
        .await?;
    let c = reply
        .get_document("cursor")
        .map_err(|_| bad("malformed getMore reply"))?;
    p.id = num(c, "id");
    if let Ok(a) = c.get_array("nextBatch") {
        p.buffer
            .extend(a.iter().filter_map(|b| b.as_document().cloned()));
    }
    Ok(())
}

/// Close a cursor on the server (best effort).
pub async fn kill_cursor(cmd: &mut Cmd<'_>, p: &Pending) {
    if p.id != 0 {
        let _ = cmd
            .run(&p.db, doc! {"killCursors": &p.coll, "cursors": [p.id]})
            .await;
    }
}

/// Send up to `limit` documents of `p` to the sink. Returns the number sent and the cursor when
/// documents remain.
pub async fn pump(
    cmd: &mut Cmd<'_>,
    ctx: &RunCtx,
    mut p: Pending,
    limit: Option<u64>,
    sink: &mut dyn ResultSink,
) -> Result<(u64, Option<Pending>)> {
    let mut sent = 0u64;
    let mut batch: Vec<Vec<Value>> = Vec::new();
    let flush = |batch: &mut Vec<Vec<Value>>, sink: &mut dyn ResultSink| -> Result<()> {
        if !batch.is_empty() {
            sink.on_rows(std::mem::take(batch))?;
        }
        Ok(())
    };
    let remaining = loop {
        let at_limit = |sent: u64| limit.is_some_and(|l| sent >= l);
        while !at_limit(sent) {
            let Some(d) = p.buffer.pop_front() else {
                break;
            };
            batch.push(vec![doc_to_value(&d)]);
            sent += 1;
            if batch.len() >= ctx.batch {
                flush(&mut batch, sink)?;
                ctx.check_cancel()?;
            }
        }
        if at_limit(sent) {
            flush(&mut batch, sink)?;
            // Is there really more? Peek one batch when nothing is buffered.
            if p.buffer.is_empty() && p.id != 0 {
                fetch_next(cmd, &mut p, ctx.batch).await?;
            }
            break !p.buffer.is_empty();
        }
        if p.id == 0 {
            flush(&mut batch, sink)?;
            break false;
        }
        flush(&mut batch, sink)?;
        ctx.check_cancel()?;
        fetch_next(cmd, &mut p, ctx.batch).await?;
    };
    if remaining {
        Ok((sent, Some(p)))
    } else {
        Ok((sent, None))
    }
}

// -- argument helpers --------------------------------------------------------------------------

fn arg_doc(args: &[ShellValue], i: usize) -> Result<Document> {
    match args.get(i) {
        None | Some(ShellValue::Null) => Ok(Document::new()),
        Some(v) => shell_to_doc(v),
    }
}

/// Filter of a write: must be an explicit document (`{}` selects everything), so a missing or
/// null filter can never turn into "all documents".
fn arg_write_filter(args: &[ShellValue], i: usize, method: &str) -> Result<Document> {
    match args.get(i) {
        None | Some(ShellValue::Null) => Err(bad(format!(
            "{method} needs an explicit filter document (use {{}} to match every document)"
        ))),
        Some(v) => shell_to_doc(v),
    }
}

/// Verbosity argument of `.explain()`.
fn explain_verbosity(args: &[ShellValue]) -> Result<&'static str> {
    match args.first() {
        None => Ok("queryPlanner"),
        Some(ShellValue::String(s)) => match s.as_str() {
            "queryPlanner" => Ok("queryPlanner"),
            "executionStats" => Ok("executionStats"),
            "allPlansExecution" => Ok("allPlansExecution"),
            other => Err(bad(format!("unknown explain verbosity '{other}'"))),
        },
        Some(ShellValue::Bool(true)) => Ok("allPlansExecution"),
        Some(ShellValue::Bool(false)) => Ok("queryPlanner"),
        _ => Err(bad("explain expects a verbosity string")),
    }
}

fn arg_opt_doc(args: &[ShellValue], i: usize) -> Result<Option<Document>> {
    match args.get(i) {
        None | Some(ShellValue::Null) => Ok(None),
        Some(v) => shell_to_doc(v).map(Some),
    }
}

fn arg_required_doc(args: &[ShellValue], i: usize, what: &str) -> Result<Document> {
    match args.get(i) {
        None => Err(bad(format!("missing {what} argument"))),
        Some(v) => shell_to_doc(v),
    }
}

fn arg_array(args: &[ShellValue], i: usize, what: &str) -> Result<Vec<Bson>> {
    match args.get(i).map(shell_to_bson).transpose()? {
        Some(Bson::Array(a)) => Ok(a),
        _ => Err(bad(format!("{what} must be an array"))),
    }
}

fn arg_i64(args: &[ShellValue], i: usize) -> Result<i64> {
    match args.get(i) {
        Some(ShellValue::Int(n)) => Ok(*n),
        Some(ShellValue::NumberInt(n)) => Ok(i64::from(*n)),
        Some(ShellValue::NumberLong(n)) => Ok(*n),
        Some(ShellValue::Double(d)) => Ok(*d as i64),
        _ => Err(bad("expected a number")),
    }
}

fn arg_str(args: &[ShellValue], i: usize, what: &str) -> Result<String> {
    match args.get(i) {
        Some(ShellValue::String(s)) => Ok(s.clone()),
        _ => Err(bad(format!("{what} must be a string"))),
    }
}

/// Copy the named keys of `opts` into `cmd`.
fn merge_opts(cmd: &mut Document, opts: &Option<Document>, keys: &[&str]) {
    if let Some(o) = opts {
        for k in keys {
            if let Some(v) = o.get(*k) {
                cmd.insert((*k).to_string(), v.clone());
            }
        }
    }
}

/// What a planned command returns and how the result is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Reply carries a cursor; rows are documents.
    Cursor,
    /// `countDocuments`: aggregation yielding `{n}`; reported as `{count: n}`.
    CountAgg,
    /// `count` / `estimatedDocumentCount` command (`n`).
    Count,
    /// `distinct` command (`values`).
    Distinct,
    /// `insert` command.
    Insert,
    /// `update` command.
    Update,
    /// `delete` command.
    Delete,
    /// `findAndModify`: reported as the `value` document.
    FindAndModify,
    /// Any other command: the reply is reported as one document.
    Command,
    /// A command that only reports success.
    Done,
}

/// A command ready to run.
#[derive(Debug, Clone)]
pub struct Planned {
    pub db: String,
    pub cmd: Document,
    pub kind: Kind,
    /// Ids reported for inserts.
    pub inserted_ids: Vec<Bson>,
}

fn collection_of(stmt: &ShellStatement) -> Result<String> {
    stmt.collection
        .clone()
        .ok_or_else(|| bad("a collection is required"))
}

/// Build the database command of a statement; `Ok(None)` for `use` / `show`.
pub fn plan(stmt: &ShellStatement, db: &str, comment: &str) -> Result<Option<Planned>> {
    let args = stmt.args.as_slice();
    let mut inserted_ids = Vec::new();
    // Chained calls only make sense on cursors; anywhere else they would be silently ignored
    // (`deleteMany({}).explain()` must never run the delete).
    let cursor_method = stmt.kind == ShellKind::Collection
        && matches!(stmt.method.as_str(), "find" | "findOne" | "aggregate");
    if !stmt.chain.is_empty() && !cursor_method {
        return Err(bad(format!(
            "'.{}()' cannot be chained after {}()",
            stmt.chain[0].0, stmt.method
        )));
    }
    let mut explain: Option<&'static str> = None;
    let (cmd_db, mut cmd, kind): (String, Document, Kind) = match stmt.kind {
        ShellKind::Use | ShellKind::Show => return Ok(None),
        ShellKind::Database => match stmt.method.as_str() {
            "runCommand" | "adminCommand" => {
                let c = match args.first() {
                    Some(ShellValue::String(s)) => doc! {s.clone(): 1},
                    _ => arg_required_doc(args, 0, "command")?,
                };
                let target = if stmt.method == "adminCommand" {
                    "admin".to_string()
                } else {
                    db.to_string()
                };
                (target, c, Kind::Command)
            }
            "getCollectionNames" => (
                db.to_string(),
                doc! {"listCollections": 1, "nameOnly": true},
                Kind::Cursor,
            ),
            "getCollectionInfos" => (db.to_string(), doc! {"listCollections": 1}, Kind::Cursor),
            "createCollection" => {
                let name = arg_str(args, 0, "collection name")?;
                let mut c = doc! {"create": name};
                if let Some(o) = arg_opt_doc(args, 1)? {
                    for (k, v) in o {
                        c.insert(k, v);
                    }
                }
                (db.to_string(), c, Kind::Done)
            }
            "createView" => {
                let name = arg_str(args, 0, "view name")?;
                let on = arg_str(args, 1, "source")?;
                let pipeline = arg_array(args, 2, "pipeline")?;
                (
                    db.to_string(),
                    doc! {"create": name, "viewOn": on, "pipeline": pipeline},
                    Kind::Done,
                )
            }
            "dropDatabase" => (db.to_string(), doc! {"dropDatabase": 1}, Kind::Done),
            "version" => (db.to_string(), doc! {"buildInfo": 1}, Kind::Command),
            "hello" | "isMaster" => (db.to_string(), doc! {"hello": 1}, Kind::Command),
            "stats" => (db.to_string(), doc! {"dbStats": 1}, Kind::Command),
            other => return Err(Error::Unsupported(format!("db.{other}()"))),
        },
        ShellKind::Collection => {
            let coll = collection_of(stmt)?;
            match stmt.method.as_str() {
                "find" | "findOne" => {
                    let mut c = doc! {"find": &coll, "filter": arg_doc(args, 0)?};
                    if let Some(p) = arg_opt_doc(args, 1)? {
                        c.insert("projection", p);
                    }
                    if stmt.method == "findOne" {
                        c.insert("limit", 1i64);
                        c.insert("singleBatch", true);
                    }
                    let mut count_only = false;
                    for (name, a) in &stmt.chain {
                        match name.as_str() {
                            "sort" => {
                                c.insert("sort", arg_doc(a, 0)?);
                            }
                            "limit" => {
                                c.insert("limit", arg_i64(a, 0)?.max(0));
                            }
                            "skip" => {
                                c.insert("skip", arg_i64(a, 0)?.max(0));
                            }
                            "projection" | "project" => {
                                c.insert("projection", arg_doc(a, 0)?);
                            }
                            "batchSize" => {
                                c.insert("batchSize", arg_i64(a, 0)?.max(1));
                            }
                            "hint" => {
                                if let Some(h) = a.first() {
                                    c.insert("hint", shell_to_bson(h)?);
                                }
                            }
                            "collation" => {
                                c.insert("collation", arg_doc(a, 0)?);
                            }
                            "maxTimeMS" => {
                                c.insert("maxTimeMS", arg_i64(a, 0)?);
                            }
                            "count" | "itcount" | "size" => count_only = true,
                            "explain" => explain = Some(explain_verbosity(a)?),
                            "toArray" | "pretty" | "forEach" | "map" => {}
                            other => {
                                return Err(Error::Unsupported(format!("cursor.{other}()")));
                            }
                        }
                    }
                    if count_only && explain.is_some() {
                        return Err(bad("count cannot be explained this way"));
                    }
                    if count_only {
                        let mut cc = doc! {"count": &coll, "query": c.get_document("filter").cloned().unwrap_or_default()};
                        for k in ["skip", "limit", "hint", "collation", "maxTimeMS"] {
                            if let Some(v) = c.get(k) {
                                cc.insert(k, v.clone());
                            }
                        }
                        (db.to_string(), cc, Kind::Count)
                    } else {
                        (db.to_string(), c, Kind::Cursor)
                    }
                }
                "aggregate" => {
                    let pipeline = arg_array(args, 0, "pipeline")?;
                    let mut c = doc! {"aggregate": &coll, "pipeline": pipeline, "cursor": {}};
                    let opts = arg_opt_doc(args, 1)?;
                    merge_opts(
                        &mut c,
                        &opts,
                        &["allowDiskUse", "collation", "hint", "maxTimeMS", "let"],
                    );
                    if let Some(b) = opts.as_ref().and_then(|o| o.get("batchSize")) {
                        c.insert("cursor", doc! {"batchSize": b.clone()});
                    }
                    for (name, a) in &stmt.chain {
                        match name.as_str() {
                            "explain" => explain = Some(explain_verbosity(a)?),
                            "toArray" | "pretty" | "forEach" | "map" => {}
                            other => {
                                return Err(Error::Unsupported(format!("cursor.{other}()")));
                            }
                        }
                    }
                    (db.to_string(), c, Kind::Cursor)
                }
                "countDocuments" => {
                    let filter = arg_doc(args, 0)?;
                    let mut pipeline = vec![];
                    if !filter.is_empty() {
                        pipeline.push(Bson::Document(doc! {"$match": filter}));
                    }
                    if let Some(o) = arg_opt_doc(args, 1)? {
                        if let Some(s) = o.get("skip") {
                            pipeline.push(Bson::Document(doc! {"$skip": s.clone()}));
                        }
                        if let Some(l) = o.get("limit") {
                            pipeline.push(Bson::Document(doc! {"$limit": l.clone()}));
                        }
                    }
                    pipeline.push(Bson::Document(
                        doc! {"$group": {"_id": 1, "n": {"$sum": 1}}},
                    ));
                    (
                        db.to_string(),
                        doc! {"aggregate": &coll, "pipeline": pipeline, "cursor": {}},
                        Kind::CountAgg,
                    )
                }
                "estimatedDocumentCount" => (db.to_string(), doc! {"count": &coll}, Kind::Count),
                "count" => {
                    let mut c = doc! {"count": &coll, "query": arg_doc(args, 0)?};
                    merge_opts(
                        &mut c,
                        &arg_opt_doc(args, 1)?,
                        &["skip", "limit", "hint", "collation"],
                    );
                    (db.to_string(), c, Kind::Count)
                }
                "distinct" => {
                    let key = arg_str(args, 0, "field")?;
                    let c = doc! {"distinct": &coll, "key": key, "query": arg_doc(args, 1)?};
                    (db.to_string(), c, Kind::Distinct)
                }
                "insertOne" => {
                    let mut d = arg_required_doc(args, 0, "document")?;
                    if !d.contains_key("_id") {
                        // Same position as the server would put it: first.
                        let mut with_id = doc! {"_id": bson::oid::ObjectId::new()};
                        for (k, v) in d {
                            with_id.insert(k, v);
                        }
                        d = with_id;
                    }
                    inserted_ids.push(d.get("_id").cloned().unwrap_or(Bson::Null));
                    (
                        db.to_string(),
                        doc! {"insert": &coll, "documents": [d]},
                        Kind::Insert,
                    )
                }
                "insertMany" => {
                    let docs = arg_array(args, 0, "documents")?;
                    let mut out = Vec::new();
                    for b in docs {
                        let Bson::Document(mut d) = b else {
                            return Err(bad("insertMany expects documents"));
                        };
                        if !d.contains_key("_id") {
                            let mut with_id = doc! {"_id": bson::oid::ObjectId::new()};
                            for (k, v) in d {
                                with_id.insert(k, v);
                            }
                            d = with_id;
                        }
                        inserted_ids.push(d.get("_id").cloned().unwrap_or(Bson::Null));
                        out.push(Bson::Document(d));
                    }
                    let ordered = arg_opt_doc(args, 1)?
                        .and_then(|o| o.get_bool("ordered").ok())
                        .unwrap_or(true);
                    (
                        db.to_string(),
                        doc! {"insert": &coll, "documents": out, "ordered": ordered},
                        Kind::Insert,
                    )
                }
                m @ ("updateOne" | "updateMany" | "replaceOne") => {
                    let q = arg_write_filter(args, 0, m)?;
                    let u = match args.get(1).map(shell_to_bson).transpose()? {
                        Some(Bson::Document(d)) => Bson::Document(d),
                        Some(Bson::Array(a)) => Bson::Array(a),
                        _ => return Err(bad("missing update argument")),
                    };
                    let opts = arg_opt_doc(args, 2)?;
                    let mut entry = doc! {"q": q, "u": u, "multi": m == "updateMany"};
                    merge_opts(&mut entry, &opts, &["upsert", "collation", "hint"]);
                    if let Some(af) = opts.as_ref().and_then(|o| o.get("arrayFilters")) {
                        entry.insert("arrayFilters", af.clone());
                    }
                    (
                        db.to_string(),
                        doc! {"update": &coll, "updates": [entry]},
                        Kind::Update,
                    )
                }
                m @ ("deleteOne" | "deleteMany") => {
                    let q = arg_write_filter(args, 0, m)?;
                    let mut entry = doc! {"q": q, "limit": if m == "deleteOne" {1} else {0}};
                    merge_opts(&mut entry, &arg_opt_doc(args, 1)?, &["collation", "hint"]);
                    (
                        db.to_string(),
                        doc! {"delete": &coll, "deletes": [entry]},
                        Kind::Delete,
                    )
                }
                m @ ("findOneAndUpdate" | "findOneAndReplace" | "findOneAndDelete") => {
                    let mut c =
                        doc! {"findAndModify": &coll, "query": arg_write_filter(args, 0, m)?};
                    let opts = if m == "findOneAndDelete" {
                        c.insert("remove", true);
                        arg_opt_doc(args, 1)?
                    } else {
                        let u = match args.get(1).map(shell_to_bson).transpose()? {
                            Some(b @ (Bson::Document(_) | Bson::Array(_))) => b,
                            _ => return Err(bad("missing update argument")),
                        };
                        c.insert("update", u);
                        arg_opt_doc(args, 2)?
                    };
                    if let Some(o) = &opts {
                        if let Some(p) = o.get("projection") {
                            c.insert("fields", p.clone());
                        }
                        if let Some(rd) = o.get("returnDocument") {
                            c.insert("new", rd.as_str() == Some("after"));
                        }
                        if let Some(nw) = o.get("returnNewDocument") {
                            c.insert("new", nw.clone());
                        }
                    }
                    merge_opts(
                        &mut c,
                        &opts,
                        &["sort", "upsert", "collation", "arrayFilters"],
                    );
                    (db.to_string(), c, Kind::FindAndModify)
                }
                "drop" => (db.to_string(), doc! {"drop": &coll}, Kind::Done),
                "createIndex" => {
                    let keys = arg_required_doc(args, 0, "keys")?;
                    let mut idx = doc! {"key": keys.clone()};
                    let opts = arg_opt_doc(args, 1)?;
                    let name = opts
                        .as_ref()
                        .and_then(|o| o.get_str("name").ok().map(str::to_string))
                        .unwrap_or_else(|| default_index_name(&keys));
                    idx.insert("name", name);
                    if let Some(o) = opts {
                        for (k, v) in o {
                            if k != "name" {
                                idx.insert(k, v);
                            }
                        }
                    }
                    let mut c = doc! {"createIndexes": &coll, "indexes": [idx]};
                    // mongosh: createIndex(keys, options, commitQuorum)
                    if let Some(q) = args.get(2).map(shell_to_bson).transpose()? {
                        c.insert("commitQuorum", q);
                    }
                    (db.to_string(), c, Kind::Command)
                }
                "dropIndex" => {
                    let index = match args.first().map(shell_to_bson).transpose()? {
                        Some(Bson::String(s)) => Bson::String(s),
                        Some(Bson::Document(d)) => Bson::Document(d),
                        _ => return Err(bad("dropIndex expects an index name or key")),
                    };
                    (
                        db.to_string(),
                        doc! {"dropIndexes": &coll, "index": index},
                        Kind::Command,
                    )
                }
                "getIndexes" => (db.to_string(), doc! {"listIndexes": &coll}, Kind::Cursor),
                "stats" => (db.to_string(), doc! {"collStats": &coll}, Kind::Command),
                "renameCollection" => {
                    let to = arg_str(args, 0, "new name")?;
                    (
                        "admin".to_string(),
                        doc! {"renameCollection": format!("{db}.{coll}"), "to": format!("{db}.{to}")},
                        Kind::Done,
                    )
                }
                other => return Err(Error::Unsupported(format!("db.collection.{other}()"))),
            }
        }
    };
    if let Some(verbosity) = explain {
        // The statement is explained, never executed.
        let wrapped = doc! {"explain": cmd, "verbosity": verbosity};
        return Ok(Some(Planned {
            db: cmd_db,
            cmd: wrapped,
            kind: Kind::Command,
            inserted_ids: vec![],
        }));
    }
    if !comment.is_empty() {
        cmd.insert("comment", comment);
    }
    Ok(Some(Planned {
        db: cmd_db,
        cmd,
        kind,
        inserted_ids,
    }))
}

/// Default index name: `a_1_b_-1`.
pub fn default_index_name(keys: &Document) -> String {
    keys.iter()
        .map(|(k, v)| {
            let d = match v {
                Bson::Int32(i) => i.to_string(),
                Bson::Int64(i) => i.to_string(),
                Bson::Double(f) => (*f as i64).to_string(),
                Bson::String(s) => s.clone(),
                _ => "1".into(),
            };
            format!("{k}_{d}")
        })
        .collect::<Vec<_>>()
        .join("_")
}

fn end_rows(sink: &mut dyn ResultSink, rows: u64, truncated: bool, started: Instant) -> Result<()> {
    sink.on_statement_end(StatementEnd {
        outcome: EndOutcome::Rows {
            row_count: rows,
            truncated,
        },
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

fn end_affected(sink: &mut dyn ResultSink, rows: u64, started: Instant) -> Result<()> {
    sink.on_statement_end(StatementEnd {
        outcome: EndOutcome::Affected { rows },
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

fn one_doc(sink: &mut dyn ResultSink, d: Document, started: Instant) -> Result<()> {
    sink.on_columns(vec![doc_column()])?;
    sink.on_rows(vec![vec![doc_to_value(&d)]])?;
    end_rows(sink, 1, false, started)
}

/// Int32 when the value fits (like the server's own counters), else Int64.
fn int_bson(n: i64) -> Bson {
    i32::try_from(n).map(Bson::Int32).unwrap_or(Bson::Int64(n))
}

fn id_text(b: &Bson) -> String {
    crate::convert::to_ejson(b, false, false)
}

/// Run one planned command and report to `sink`. Returns the cursor to keep, if requested and
/// documents remain.
pub async fn run_planned(
    cmd: &mut Cmd<'_>,
    ctx: &RunCtx,
    plan: Planned,
    keep_cursor: bool,
    sink: &mut dyn ResultSink,
) -> Result<Option<Pending>> {
    let started = Instant::now();
    let Planned {
        db,
        cmd: command,
        kind,
        inserted_ids,
    } = plan;
    // Make the first batch small enough that `max_rows` is respected cheaply.
    let reply = cmd.run(&db, command).await?;
    match kind {
        Kind::Cursor => {
            let p = pending_from_reply(&reply, &db)?;
            sink.on_columns(vec![doc_column()])?;
            let (n, rest) = match pump(cmd, ctx, p, ctx.max_rows, sink).await {
                Ok(r) => r,
                Err(e) => return Err(e),
            };
            end_rows(sink, n, rest.is_some(), started)?;
            match rest {
                Some(p) if keep_cursor => Ok(Some(p)),
                Some(p) => {
                    kill_cursor(cmd, &p).await;
                    Ok(None)
                }
                None => Ok(None),
            }
        }
        Kind::CountAgg => {
            let p = pending_from_reply(&reply, &db)?;
            let n = p.buffer.front().map(|d| num(d, "n")).unwrap_or(0);
            one_doc(sink, doc! {"count": int_bson(n)}, started)?;
            Ok(None)
        }
        Kind::Count => {
            one_doc(sink, doc! {"count": int_bson(num(&reply, "n"))}, started)?;
            Ok(None)
        }
        Kind::Distinct => {
            let values = reply.get_array("values").cloned().unwrap_or_default();
            one_doc(sink, doc! {"values": values}, started)?;
            Ok(None)
        }
        Kind::Insert => {
            let n = num(&reply, "n");
            if !inserted_ids.is_empty() {
                let list: Vec<String> = inserted_ids.iter().map(id_text).collect();
                sink.on_message(format!("insertedIds: [{}]", list.join(", ")))?;
            }
            end_affected(sink, n.max(0) as u64, started)?;
            Ok(None)
        }
        Kind::Update => {
            let matched = num(&reply, "n");
            let modified = num(&reply, "nModified");
            let upserted = reply.get_array("upserted").map(|a| a.len()).unwrap_or(0) as i64;
            let mut msg = format!("matched: {}, modified: {modified}", matched - upserted);
            if let Ok(a) = reply.get_array("upserted") {
                if let Some(Bson::Document(u)) = a.first() {
                    if let Some(id) = u.get("_id") {
                        msg.push_str(&format!(", upsertedId: {}", id_text(id)));
                    }
                }
            }
            sink.on_message(msg)?;
            end_affected(sink, (modified + upserted).max(0) as u64, started)?;
            Ok(None)
        }
        Kind::Delete => {
            end_affected(sink, num(&reply, "n").max(0) as u64, started)?;
            Ok(None)
        }
        Kind::FindAndModify => {
            match reply.get("value") {
                Some(Bson::Document(d)) => one_doc(sink, d.clone(), started)?,
                _ => sink.on_statement_end(StatementEnd {
                    outcome: EndOutcome::Done,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                })?,
            }
            Ok(None)
        }
        Kind::Command => {
            one_doc(sink, reply, started)?;
            Ok(None)
        }
        Kind::Done => {
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Done,
                elapsed_ms: started.elapsed().as_millis() as u64,
            })?;
            Ok(None)
        }
    }
}

/// Emit a result set of single-column text rows (`show ...`).
pub fn names_result(sink: &mut dyn ResultSink, column: &str, names: Vec<String>) -> Result<()> {
    let started = Instant::now();
    sink.on_columns(vec![ColumnMeta {
        name: column.into(),
        type_name: "string".into(),
        value_kind: ValueKind::Text,
        nullable: Some(false),
    }])?;
    let n = names.len() as u64;
    if !names.is_empty() {
        sink.on_rows(names.into_iter().map(|s| vec![Value::Text(s)]).collect())?;
    }
    end_rows(sink, n, false, started)
}

/// Statement bookkeeping shared by sessions and leases.
pub fn statement_start(
    sink: &mut dyn ResultSink,
    index: usize,
    stmt: &ShellStatement,
) -> Result<()> {
    sink.on_statement_start(StatementStart {
        index: index as u32,
        range: stmt.range,
    })
}

/// Run every statement of a parsed script. `db` is updated by `use`. `keep_last` keeps the
/// cursor of the last statement (leases).
pub async fn run_script(
    cmd: &mut Cmd<'_>,
    ctx: &RunCtx,
    db: &mut String,
    statements: &[ShellStatement],
    keep_last: bool,
    sink: &mut dyn ResultSink,
) -> Result<(u32, Option<Pending>)> {
    let comment = ctx.query_id.to_string();
    let mut pending = None;
    for (i, stmt) in statements.iter().enumerate() {
        ctx.check_cancel()?;
        statement_start(sink, i, stmt)?;
        let last = i + 1 == statements.len();
        match stmt.kind {
            ShellKind::Use => {
                *db = stmt.method.clone();
                sink.on_message(format!("switched to db {db}"))?;
                sink.on_statement_end(StatementEnd {
                    outcome: EndOutcome::Done,
                    elapsed_ms: 0,
                })?;
            }
            ShellKind::Show => {
                show(cmd, db, &stmt.method, sink).await?;
            }
            _ => {
                if ctx.read_only {
                    use dbstudio_core::StatementClass as C;
                    let (class, _) = dbstudio_sql::mongo_shell::classify(stmt);
                    let allowed = match class {
                        C::Read | C::Session | C::Empty => true,
                        C::Explain { analyze, writes } => !(analyze && writes),
                        _ => false,
                    };
                    if !allowed {
                        return Err(Error::ReadOnlyViolation {
                            message: "the connection is read-only".into(),
                            position: Some(stmt.range.start),
                        });
                    }
                }
                // `db.getSiblingDB('x').…` runs on x, whatever `use` selected.
                let target = stmt.database.clone().unwrap_or_else(|| db.clone());
                let planned =
                    plan(stmt, &target, &comment)?.ok_or_else(|| bad("empty statement"))?;
                pending = run_planned(cmd, ctx, planned, keep_last && last, sink)
                    .await
                    .map_err(|e| {
                        if ctx.cancel.load(Ordering::Acquire) {
                            Error::Cancelled
                        } else {
                            e
                        }
                    })?;
            }
        }
    }
    Ok((statements.len() as u32, pending))
}

async fn show(cmd: &mut Cmd<'_>, db: &str, what: &str, sink: &mut dyn ResultSink) -> Result<()> {
    match what.to_ascii_lowercase().as_str() {
        "dbs" | "databases" => {
            let r = cmd
                .run("admin", doc! {"listDatabases": 1, "nameOnly": true})
                .await?;
            let names = r
                .get_array("databases")
                .map(|a| {
                    a.iter()
                        .filter_map(|d| d.as_document()?.get_str("name").ok().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            names_result(sink, "name", names)
        }
        "collections" | "tables" => {
            let r = cmd
                .run(db, doc! {"listCollections": 1, "nameOnly": true})
                .await?;
            let p = pending_from_reply(&r, db)?;
            let mut names: Vec<String> = p
                .buffer
                .iter()
                .filter_map(|d| d.get_str("name").ok().map(str::to_string))
                .collect();
            names.sort();
            names_result(sink, "name", names)
        }
        "users" => {
            let r = cmd.run(db, doc! {"usersInfo": 1}).await?;
            let names = r
                .get_array("users")
                .map(|a| {
                    a.iter()
                        .filter_map(|d| d.as_document()?.get_str("user").ok().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            names_result(sink, "name", names)
        }
        other => Err(Error::Unsupported(format!("show {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_sql::mongo_shell;

    fn planned(text: &str) -> Result<Option<Planned>> {
        // The parser already refuses unknown chains; the driver refuses them again (defence).
        let stmts = mongo_shell::parse(text)?;
        plan(&stmts[0], "app", "cid")
    }

    #[test]
    fn chains_on_writes_are_rejected_not_ignored() {
        for text in [
            "db.c.deleteMany({}).explain()",
            "db.c.updateOne({a:1},{$set:{b:1}}).limit(1)",
            "db.c.drop().explain()",
            "db.c.insertOne({a:1}).sort({a:1})",
            "db.runCommand({ping:1}).explain()",
        ] {
            let e = planned(text).unwrap_err();
            assert_eq!(e.kind(), "query", "{text}");
            let m = e.message();
            assert!(
                m.contains("cannot be chained") || m.contains("cannot follow"),
                "{text}: {e}"
            );
        }
        // A plain write still plans.
        assert!(planned("db.c.deleteMany({})").unwrap().is_some());
    }

    #[test]
    fn explain_chain_on_cursors_wraps_and_never_executes() {
        let p = planned("db.c.aggregate([{$match:{a:1}},{$out:'t'}]).explain()")
            .unwrap()
            .unwrap();
        assert_eq!(p.kind, Kind::Command);
        assert!(p.cmd.contains_key("explain") && !p.cmd.contains_key("aggregate"));
        assert_eq!(p.cmd.get_str("verbosity").unwrap(), "queryPlanner");
        let p = planned("db.c.find({a:1}).sort({a:1}).explain('executionStats')")
            .unwrap()
            .unwrap();
        assert_eq!(p.cmd.get_str("verbosity").unwrap(), "executionStats");
        assert!(p.cmd.get_document("explain").unwrap().contains_key("find"));
        assert!(planned("db.c.find({}).explain('weird')").is_err());
        assert!(planned("db.c.aggregate([]).sort({a:1})").is_err());
    }

    #[test]
    fn write_filters_must_be_explicit_documents() {
        for text in [
            "db.c.deleteMany()",
            "db.c.deleteOne(null)",
            "db.c.updateMany(null, {$set:{a:1}})",
            "db.c.updateOne(undefined, {$set:{a:1}})",
            "db.c.replaceOne(null, {a:1})",
            "db.c.findOneAndDelete()",
            "db.c.findOneAndUpdate(null, {$set:{a:1}})",
        ] {
            let e = planned(text).unwrap_err();
            assert!(e.message().contains("explicit filter"), "{text}: {e}");
        }
        assert!(planned("db.c.deleteMany({})").is_ok());
        assert!(planned("db.c.updateMany({}, {$set:{a:1}})").is_ok());
        // Reads keep treating a missing filter as "everything".
        assert!(planned("db.c.find()").is_ok());
        assert!(planned("db.c.find(null)").is_ok());
        assert!(planned("db.c.countDocuments()").is_ok());
    }

    #[test]
    fn commands_carry_the_query_id_comment() {
        for text in [
            "db.c.drop()",
            "db.createCollection('x')",
            "db.dropDatabase()",
            "db.c.find({})",
        ] {
            let p = planned(text).unwrap().unwrap();
            assert_eq!(p.cmd.get_str("comment").unwrap(), "cid", "{text}");
        }
    }
}
