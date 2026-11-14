//! Lease: a dedicated client (own connections) with a causal session; transactions on replica
//! sets and mongos.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use bson::{Bson, Document, doc};
use dbstudio_core::{
    BulkInsertRequest, BulkInsertResult, EditValue, EndOutcome, Error, ExecSummary, ExplainRequest,
    Lease, LeaseCloseReport, LeaseContext, LeaseOptions, LeaseState, OnConflict, QueryRequest,
    RawPlan, Result, ResultSink, StatementEnd, TableStreamRequest, TxOptions, TxStatus, Value,
};
use dbstudio_sql::mongo_shell;
use mongodb::{Client, ClientSession};
use uuid::Uuid;

use crate::conn::{Cmd, Conn, check_write_errors, map_error, num};
use crate::convert::{parse_ejson_doc, parse_ejson_value, value_to_bson};
use crate::exec::{Pending, doc_column, kill_cursor, pending_from_reply, pump, run_script};
use crate::query::{build_filter, build_sort, resolve};
use crate::session::{
    Registered, Running, explain_statement, find_command, kill_op, run_ctx, with_timeout,
};

struct Inner {
    session: ClientSession,
    db: String,
    auto_commit: bool,
    in_txn: bool,
    pending: Option<(Uuid, Pending)>,
}

/// A lease on a MongoDB server.
pub struct MongoLease {
    conn: Arc<Conn>,
    client: Client,
    inner: tokio::sync::Mutex<Inner>,
    state: Mutex<LeaseState>,
    running: Running,
}

fn bad(msg: impl Into<String>) -> Error {
    Error::Query {
        message: msg.into(),
        position: None,
    }
}

impl MongoLease {
    pub async fn open(conn: Arc<Conn>, opts: LeaseOptions) -> Result<MongoLease> {
        let client = conn.dedicated_client().await?;
        let session = client.start_session().await.map_err(map_error)?;
        let db = opts.database.clone().unwrap_or_else(|| conn.default_db());
        let state = LeaseState {
            auto_commit: opts.auto_commit,
            transaction: TxStatus::Idle,
            running: None,
            pending_result: None,
            database: Some(db.clone()),
            schema: None,
            broken: false,
        };
        Ok(MongoLease {
            conn,
            client,
            inner: tokio::sync::Mutex::new(Inner {
                session,
                db,
                auto_commit: opts.auto_commit,
                in_txn: false,
                pending: None,
            }),
            state: Mutex::new(state),
            running: Running::default(),
        })
    }

    fn set_state(&self, f: impl FnOnce(&mut LeaseState)) {
        f(&mut self.state.lock().unwrap());
    }

    fn lock(&self) -> Result<tokio::sync::MutexGuard<'_, Inner>> {
        self.inner
            .try_lock()
            .map_err(|_| Error::Busy("the lease is running a statement".into()))
    }

    fn require_transactions(&self) -> Result<()> {
        if self.conn.transactions {
            Ok(())
        } else {
            Err(Error::Unsupported("transactions_standalone".into()))
        }
    }

    async fn drop_pending(&self, inner: &mut Inner) {
        if let Some((_, p)) = inner.pending.take() {
            let mut cmd = Cmd {
                client: &self.client,
                session: Some(&mut inner.session),
            };
            kill_cursor(&mut cmd, &p).await;
        }
        self.set_state(|s| s.pending_result = None);
    }

    async fn ensure_txn(&self, inner: &mut Inner) -> Result<()> {
        if !inner.auto_commit && !inner.in_txn {
            self.require_transactions()?;
            inner.session.start_transaction().await.map_err(map_error)?;
            inner.in_txn = true;
            self.set_state(|s| s.transaction = TxStatus::Active);
        }
        Ok(())
    }
}

#[async_trait]
impl Lease for MongoLease {
    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        let started = Instant::now();
        let mut guard = self.lock()?;
        let inner = &mut *guard;
        self.drop_pending(inner).await;
        let statements = mongo_shell::parse(&req.text)?;
        self.ensure_txn(inner).await?;
        let reg = Registered::new(&self.running, req.query_id);
        self.set_state(|s| s.running = Some(req.query_id));
        let mut ctx = run_ctx(req.query_id, req.max_rows, req.batch_size, &reg);
        ctx.read_only = self.conn.read_only();
        let mut db = inner.db.clone();
        let result = {
            let fut = async {
                let mut cmd = Cmd {
                    client: &self.client,
                    session: Some(&mut inner.session),
                };
                run_script(&mut cmd, &ctx, &mut db, &statements, true, sink).await
            };
            with_timeout(
                req.timeout_ms,
                &self.client,
                &self.running,
                &reg,
                req.query_id,
                fut,
            )
            .await
        };
        self.set_state(|s| s.running = None);
        match result {
            Ok((n, pending)) => {
                inner.db = db.clone();
                let has_pending = pending.is_some();
                inner.pending = pending.map(|p| (req.query_id, p));
                self.set_state(|s| {
                    s.database = Some(db);
                    s.pending_result = has_pending.then_some(req.query_id);
                });
                Ok(ExecSummary {
                    statements: n,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                })
            }
            Err(e) => {
                if inner.in_txn && !matches!(e, Error::Cancelled) {
                    // The server aborts the transaction on most command errors.
                    self.set_state(|s| s.transaction = TxStatus::Failed);
                }
                Err(e)
            }
        }
    }

    async fn fetch_more(
        &self,
        query_id: Uuid,
        max_rows: u64,
        batch_size: Option<u32>,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let started = Instant::now();
        let mut guard = self.lock()?;
        let inner = &mut *guard;
        let Some((id, pending)) = inner.pending.take() else {
            return Err(Error::NotFound("no pending result".into()));
        };
        if id != query_id {
            inner.pending = Some((id, pending));
            return Err(Error::NotFound("no such pending result".into()));
        }
        let reg = Registered::new(&self.running, query_id);
        self.set_state(|s| s.running = Some(query_id));
        let ctx = run_ctx(query_id, Some(max_rows), batch_size, &reg);
        let r = {
            let fut = async {
                let mut cmd = Cmd {
                    client: &self.client,
                    session: Some(&mut inner.session),
                };
                pump(&mut cmd, &ctx, pending, Some(max_rows), sink).await
            };
            with_timeout(None, &self.client, &self.running, &reg, query_id, fut).await
        };
        self.set_state(|s| s.running = None);
        let (n, rest) = r?;
        let truncated = rest.is_some();
        inner.pending = rest.map(|p| (query_id, p));
        self.set_state(|s| s.pending_result = truncated.then_some(query_id));
        sink.on_statement_end(StatementEnd {
            outcome: EndOutcome::Rows {
                row_count: n,
                truncated,
            },
            elapsed_ms: started.elapsed().as_millis() as u64,
        })?;
        Ok(ExecSummary {
            statements: 1,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    async fn discard_result(&self) -> Result<()> {
        let mut guard = self.lock()?;
        self.drop_pending(&mut guard).await;
        Ok(())
    }

    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        let flag = self.running.lock().unwrap().get(&query_id).cloned();
        let Some(flag) = flag else {
            return Ok(());
        };
        flag.store(true, Ordering::Release);
        kill_op(&self.client, &self.running, query_id).await
    }

    async fn begin(&self, _opts: TxOptions) -> Result<()> {
        self.require_transactions()?;
        let mut guard = self.lock()?;
        let inner = &mut *guard;
        self.drop_pending(inner).await;
        if inner.in_txn {
            return Err(Error::Busy("a transaction is already open".into()));
        }
        inner.session.start_transaction().await.map_err(map_error)?;
        inner.in_txn = true;
        self.set_state(|s| s.transaction = TxStatus::Active);
        Ok(())
    }

    async fn commit(&self) -> Result<()> {
        let mut guard = self.lock()?;
        let inner = &mut *guard;
        self.drop_pending(inner).await;
        if inner.in_txn {
            inner.in_txn = false;
            self.set_state(|s| s.transaction = TxStatus::Idle);
            inner
                .session
                .commit_transaction()
                .await
                .map_err(map_error)?;
        }
        Ok(())
    }

    async fn rollback(&self) -> Result<()> {
        let mut guard = self.lock()?;
        let inner = &mut *guard;
        self.drop_pending(inner).await;
        if inner.in_txn {
            inner.in_txn = false;
            self.set_state(|s| s.transaction = TxStatus::Idle);
            inner.session.abort_transaction().await.map_err(map_error)?;
        }
        Ok(())
    }

    async fn set_auto_commit(&self, on: bool) -> Result<()> {
        let mut guard = self.lock()?;
        let inner = &mut *guard;
        if on && inner.in_txn {
            return Err(Error::Busy("a transaction is open".into()));
        }
        if !on {
            self.require_transactions()?;
        }
        inner.auto_commit = on;
        self.set_state(|s| s.auto_commit = on);
        Ok(())
    }

    async fn set_context(&self, ctx: LeaseContext) -> Result<()> {
        let mut guard = self.lock()?;
        let inner = &mut *guard;
        self.drop_pending(inner).await;
        if let Some(db) = ctx.database {
            inner.db = db.clone();
            self.set_state(|s| s.database = Some(db));
        }
        Ok(())
    }

    fn state(&self) -> LeaseState {
        self.state.lock().unwrap().clone()
    }

    async fn ping(&self) -> Result<()> {
        Cmd::pooled(&self.client)
            .run("admin", doc! {"ping": 1})
            .await
            .map(|_| ())
    }

    async fn explain(&self, req: ExplainRequest) -> Result<RawPlan> {
        let mut guard = self.lock()?;
        let inner = &mut *guard;
        let mut cmd = Cmd {
            client: &self.client,
            session: None,
        };
        let db = inner.db.clone();
        explain_statement(&mut cmd, &self.conn, &req, &db).await
    }

    async fn stream_table(
        &self,
        req: TableStreamRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let started = Instant::now();
        let mut guard = self.lock()?;
        let inner = &mut *guard;
        let (db, coll) = resolve(&req.table).ok_or_else(|| Error::NotFound("database".into()))?;
        let (coll, _) = coll.ok_or_else(|| Error::NotFound("collection".into()))?;
        let filter = build_filter(&req.filters, req.raw_where.as_deref())?;
        let sort = build_sort(&[], req.order_by_key);
        let mut command = find_command(&coll, filter, sort, 0, 0, req.batch_size, req.query_id);
        if !req.columns.is_empty() {
            let mut p = Document::new();
            for c in &req.columns {
                p.insert(c.clone(), 1);
            }
            command.insert("projection", p);
        }
        let reg = Registered::new(&self.running, req.query_id);
        let ctx = run_ctx(req.query_id, None, req.batch_size, &reg);
        let fut = async {
            let mut cmd = Cmd {
                client: &self.client,
                session: Some(&mut inner.session),
            };
            let reply = cmd.run(&db, command).await?;
            let p = pending_from_reply(&reply, &db)?;
            sink.on_columns(vec![doc_column()])?;
            let (n, rest) = pump(&mut cmd, &ctx, p, None, sink).await?;
            if let Some(p) = rest {
                kill_cursor(&mut cmd, &p).await;
            }
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Rows {
                    row_count: n,
                    truncated: false,
                },
                elapsed_ms: started.elapsed().as_millis() as u64,
            })?;
            Ok(ExecSummary {
                statements: 1,
                elapsed_ms: started.elapsed().as_millis() as u64,
            })
        };
        with_timeout(None, &self.client, &self.running, &reg, req.query_id, fut).await
    }

    async fn bulk_insert(&self, req: BulkInsertRequest) -> Result<BulkInsertResult> {
        if self.conn.read_only() {
            return Err(Error::ReadOnlyViolation {
                message: "the connection is read-only".into(),
                position: None,
            });
        }
        let mut guard = self.lock()?;
        let inner = &mut *guard;
        self.ensure_txn(inner).await?;
        let (db, coll) = resolve(&req.table).ok_or_else(|| Error::NotFound("database".into()))?;
        let (coll, _) = coll.ok_or_else(|| Error::NotFound("collection".into()))?;
        // `columns == ["$document"]`: every row is one whole document (JSON/NDJSON import,
        // backup restore): `Value::Document`, or EJSON text (relaxed or canonical).
        let whole = req.columns.len() == 1 && req.columns[0] == "$document";
        let mut docs = Vec::with_capacity(req.rows.len());
        for (n, row) in req.rows.iter().enumerate() {
            if whole {
                docs.push(whole_document(row, n)?);
                continue;
            }
            let mut d = Document::new();
            for (col, v) in req.columns.iter().zip(row) {
                match v {
                    EditValue::Default => {}
                    EditValue::Value(v) => {
                        d.insert(col.clone(), value_to_bson(v)?);
                    }
                    EditValue::Text(t) => {
                        // Typed text: a JSON literal when it is one, otherwise a string.
                        let b = parse_ejson_value(t)
                            .ok()
                            .filter(|b| !matches!(b, Bson::Document(_) | Bson::Array(_)))
                            .unwrap_or_else(|| Bson::String(t.clone()));
                        d.insert(col.clone(), b);
                    }
                }
            }
            docs.push(d);
        }
        let mut cmd = Cmd {
            client: &self.client,
            session: Some(&mut inner.session),
        };
        let mut result = BulkInsertResult::default();
        match req.on_conflict {
            OnConflict::Error => {
                for (c, chunk) in docs.chunks(1000).enumerate() {
                    let r = cmd
                        .run_unchecked(
                            &db,
                            doc! {"insert": &coll, "documents": chunk.to_vec(), "ordered": true},
                        )
                        .await?;
                    result.inserted += num(&r, "n").max(0) as u64;
                    if let Ok(errs) = r.get_array("writeErrors") {
                        if let Some(Bson::Document(e)) = errs.first() {
                            let row = c * 1000 + num(e, "index").max(0) as usize + 1;
                            let msg = e.get_str("errmsg").unwrap_or("write error");
                            return Err(bad(format!("row {row}: {msg} (code {})", num(e, "code"))));
                        }
                    }
                    check_write_errors(&r)?;
                }
            }
            OnConflict::Skip => {
                for chunk in docs.chunks(1000) {
                    let reply = cmd
                        .run_unchecked(
                            &db,
                            doc! {"insert": &coll, "documents": chunk.to_vec(), "ordered": false},
                        )
                        .await?;
                    let n = num(&reply, "n").max(0) as u64;
                    if let Ok(errs) = reply.get_array("writeErrors") {
                        for e in errs {
                            let code = e.as_document().map(|d| num(d, "code")).unwrap_or(0);
                            if code != 11000 {
                                return Err(bad(format!("insert failed: {e}")));
                            }
                        }
                        result.skipped += errs.len() as u64;
                    }
                    result.inserted += n;
                }
            }
            OnConflict::Update => {
                let keys: Vec<String> = if req.conflict_columns.is_empty() {
                    vec!["_id".into()]
                } else {
                    req.conflict_columns.clone()
                };
                if let Some(k) = keys.iter().find(|k| k.contains('.') || k.starts_with('$')) {
                    return Err(bad(format!(
                        "conflict column `{k}` is not supported (nested or operator names)"
                    )));
                }
                if whole {
                    // A replacement document cannot start with an operator ($...) field.
                    if let Some(i) = docs
                        .iter()
                        .position(|d| d.keys().next().is_some_and(|k| k.starts_with('$')))
                    {
                        return Err(bad(format!(
                            "row {}: a document cannot start with an operator ($...) field",
                            i + 1
                        )));
                    }
                }
                for chunk in docs.chunks(500) {
                    let updates: Vec<Bson> = chunk
                        .iter()
                        .map(|d| {
                            let mut q = Document::new();
                            for k in &keys {
                                if let Some(v) = d.get(k) {
                                    q.insert(k.clone(), v.clone());
                                }
                            }
                            // An empty filter would overwrite an arbitrary document.
                            if q.len() != keys.len() {
                                return Err(bad(format!(
                                    "a document lacks a value for the conflict key ({})",
                                    keys.join(", ")
                                )));
                            }
                            // Whole documents replace (`replaceOne` + upsert); column rows
                            // only set the given fields.
                            let u = if whole {
                                Bson::Document(d.clone())
                            } else {
                                let mut set = d.clone();
                                if keys.iter().any(|k| k == "_id") {
                                    set.remove("_id");
                                }
                                Bson::Document(doc! {"$set": set})
                            };
                            Ok(Bson::Document(
                                doc! {"q": q, "u": u, "upsert": true, "multi": false},
                            ))
                        })
                        .collect::<Result<_>>()?;
                    let r = cmd
                        .run(
                            &db,
                            doc! {"update": &coll, "updates": updates, "ordered": true},
                        )
                        .await?;
                    let upserted = r.get_array("upserted").map(|a| a.len()).unwrap_or(0) as u64;
                    let matched = num(&r, "n").max(0) as u64;
                    result.inserted += upserted;
                    result.updated += matched.saturating_sub(upserted);
                }
            }
        }
        Ok(result)
    }

    async fn close(&self) -> Result<LeaseCloseReport> {
        // Cancel whatever is running, then take the lock.
        let ids: Vec<Uuid> = self.running.lock().unwrap().keys().copied().collect();
        for id in ids {
            let _ = self.cancel(id).await;
        }
        let mut inner = tokio::time::timeout(std::time::Duration::from_secs(5), self.inner.lock())
            .await
            .map_err(|_| Error::Busy("the lease is still busy".into()))?;
        self.drop_pending(&mut inner).await;
        let mut rolled_back = false;
        let mut forced = false;
        if inner.in_txn {
            inner.in_txn = false;
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                inner.session.abort_transaction(),
            )
            .await
            {
                Ok(_) => rolled_back = true,
                Err(_) => forced = true,
            }
        }
        self.set_state(|s| {
            s.transaction = TxStatus::Idle;
            s.broken = true;
        });
        Ok(LeaseCloseReport {
            rolled_back,
            forced,
        })
    }
}

/// The document of a `$document` row: a `Document` value, or EJSON text.
fn whole_document(row: &[EditValue], n: usize) -> Result<Document> {
    let [cell] = row else {
        return Err(bad(format!("row {}: expected one `$document` cell", n + 1)));
    };
    let value = match cell {
        EditValue::Value(v) => v,
        _ => {
            return Err(bad(format!(
                "row {}: `$document` needs a document value",
                n + 1
            )));
        }
    };
    match value {
        Value::Json(t) | Value::Text(t) => parse_ejson_doc(t),
        other => match value_to_bson(other)? {
            Bson::Document(d) => Ok(d),
            _ => Err(bad(format!("row {}: not a document", n + 1))),
        },
    }
    .map_err(|e| bad(format!("row {}: {}", n + 1, e.message())))
}
