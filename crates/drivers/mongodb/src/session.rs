//! The MongoDB session: object tree, table browsing, scripts, explain, model and DDL.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bson::{Bson, Document, doc};
use dbstudio_core::{
    ApplyResult, ChangeSet, DdlItem, DdlRequest, Editability, Error, ExecSummary, ExplainRequest,
    IndexDetail, Lease, LeaseOptions, NotEditableReason, ObjectKind, ObjectNode, ObjectPath,
    ObjectSearchRequest, QueryRequest, RawPlan, Result, ResultSink, SchemaModel,
    SchemaModelRequest, ScriptDialect, ServerInfo, Session, TableCountRequest, TableDataRequest,
    TableDetails, TableModel, folder,
};
use dbstudio_sql::mongo_shell;
use mongodb::Client;
use uuid::Uuid;

use crate::conn::{Cmd, Conn, map_error, num};
use crate::convert::doc_to_ejson;
use crate::exec::{
    DEFAULT_BATCH, Kind, RunCtx, doc_column, kill_cursor, pending_from_reply, plan, pump,
    run_script,
};
use crate::model::{
    ddl_for, ddl_item, index_columns, infer_columns, infer_relations, list_collections,
    list_indexes, sample,
};
use crate::query::{build_filter, build_sort, resolve};

const MAX_TABLES: usize = 2000;
const COUNT_LOOKUPS: usize = 200;

pub type Running = Arc<Mutex<HashMap<Uuid, Arc<AtomicBool>>>>;

/// Registration of a running request; removed on drop.
pub struct Registered {
    running: Running,
    id: Uuid,
    pub flag: Arc<AtomicBool>,
}

impl Registered {
    pub fn new(running: &Running, id: Uuid) -> Registered {
        let flag = Arc::new(AtomicBool::new(false));
        running.lock().unwrap().insert(id, flag.clone());
        Registered {
            running: running.clone(),
            id,
            flag,
        }
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.running.lock().unwrap().remove(&self.id);
    }
}

fn bad(msg: impl Into<String>) -> Error {
    Error::Query {
        message: msg.into(),
        position: None,
    }
}

/// Kill the server operation tagged with `comment = query_id` (retries while it starts up).
pub async fn kill_op(client: &Client, running: &Running, query_id: Uuid) -> Result<()> {
    let id = query_id.to_string();
    let admin = client.database("admin");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let r = admin
            .run_command(doc! {
                "aggregate": 1,
                "pipeline": [
                    {"$currentOp": {"allUsers": false, "idleConnections": false}},
                    {"$match": {"$or": [{"command.comment": &id}, {"originatingCommand.comment": &id}]}}
                ],
                "cursor": {}
            })
            .await
            .map_err(map_error)?;
        let ops: Vec<Bson> = r
            .get_document("cursor")
            .and_then(|c| c.get_array("firstBatch"))
            .map(|a| {
                a.iter()
                    .filter_map(|d| d.as_document()?.get("opid").cloned())
                    .collect()
            })
            .unwrap_or_default();
        if !ops.is_empty() {
            for op in ops {
                let _ = admin.run_command(doc! {"killOp": 1, "op": op}).await;
            }
            return Ok(());
        }
        if !running.lock().unwrap().contains_key(&query_id) || Instant::now() > deadline {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
}

/// Run `fut` with the request timeout; a timeout cancels the server operation.
pub async fn with_timeout<T>(
    timeout_ms: Option<u64>,
    client: &Client,
    running: &Running,
    reg: &Registered,
    query_id: Uuid,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let result = match timeout_ms {
        Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), fut).await {
            Ok(r) => r,
            Err(_) => {
                reg.flag.store(true, Ordering::Release);
                let _ = kill_op(client, running, query_id).await;
                return Err(Error::Timeout);
            }
        },
        None => fut.await,
    };
    match result {
        Err(e) if reg.flag.load(Ordering::Acquire) && !matches!(e, Error::Timeout) => {
            Err(Error::Cancelled)
        }
        other => other,
    }
}

/// Build the `RunCtx` of a request.
pub fn run_ctx(
    req_id: Uuid,
    max_rows: Option<u64>,
    batch: Option<u32>,
    reg: &Registered,
) -> RunCtx {
    RunCtx {
        query_id: req_id,
        max_rows,
        batch: batch.map(|b| b.max(1) as usize).unwrap_or(DEFAULT_BATCH),
        cancel: reg.flag.clone(),
        read_only: false,
    }
}

fn human_count(n: i64) -> String {
    match n {
        n if n < 1000 => format!("~{n} docs"),
        n if n < 1_000_000 => format!("~{:.1}k docs", n as f64 / 1000.0),
        n => format!("~{:.1}M docs", n as f64 / 1_000_000.0),
    }
}

/// The MongoDB session.
impl std::fmt::Debug for MongoSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MongoSession").finish()
    }
}

pub struct MongoSession {
    pub(crate) conn: Arc<Conn>,
    pub(crate) running: Running,
}

impl MongoSession {
    pub fn new(conn: Conn) -> Self {
        MongoSession {
            conn: Arc::new(conn),
            running: Arc::default(),
        }
    }

    fn client(&self) -> &Client {
        &self.conn.client
    }

    fn db_path(db: &str) -> ObjectPath {
        ObjectPath::root().child(ObjectKind::Database, db)
    }

    async fn database_names(&self) -> Result<Vec<String>> {
        let mut cmd = Cmd::pooled(self.client());
        match cmd
            .run("admin", doc! {"listDatabases": 1, "nameOnly": true})
            .await
        {
            Ok(r) => {
                let mut names: Vec<String> = r
                    .get_array("databases")
                    .map(|a| {
                        a.iter()
                            .filter_map(|d| {
                                d.as_document()?.get_str("name").ok().map(str::to_string)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                names.sort();
                Ok(names)
            }
            // Not allowed to list databases: show the configured one only.
            Err(Error::Permission(_)) | Err(Error::Query { .. }) => Ok(self
                .conn
                .config
                .database
                .clone()
                .filter(|d| !d.is_empty())
                .into_iter()
                .collect()),
            Err(e) => Err(e),
        }
    }

    async fn collection_counts(&self, db: &str, names: &[String]) -> HashMap<String, i64> {
        let mut set = tokio::task::JoinSet::new();
        for name in names.iter().take(COUNT_LOOKUPS) {
            let client = self.client().clone();
            let (db, name) = (db.to_string(), name.clone());
            set.spawn(async move {
                let r = client
                    .database(&db)
                    .run_command(doc! {"count": &name})
                    .await
                    .ok()
                    .map(|d| num(&d, "n"));
                (name, r)
            });
        }
        let mut out = HashMap::new();
        while let Some(Ok((name, n))) = set.join_next().await {
            if let Some(n) = n {
                out.insert(name, n);
            }
        }
        out
    }

    async fn find_stream(
        &self,
        query_id: Uuid,
        db: &str,
        command: Document,
        sink: &mut dyn ResultSink,
        batch: Option<u32>,
    ) -> Result<ExecSummary> {
        let started = Instant::now();
        let reg = Registered::new(&self.running, query_id);
        let ctx = run_ctx(query_id, None, batch, &reg);
        let fut = async {
            let mut cmd = Cmd::pooled(self.client());
            let reply = cmd.run(db, command).await?;
            let p = pending_from_reply(&reply, db)?;
            sink.on_columns(vec![doc_column()])?;
            let (n, rest) = pump(&mut cmd, &ctx, p, None, sink).await?;
            if let Some(p) = rest {
                kill_cursor(&mut cmd, &p).await;
            }
            sink.on_statement_end(dbstudio_core::StatementEnd {
                outcome: dbstudio_core::EndOutcome::Rows {
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
        with_timeout(None, self.client(), &self.running, &reg, query_id, fut).await
    }
}

/// `find` command for a browser request.
pub fn find_command(
    coll: &str,
    filter: Document,
    sort: Document,
    skip: u64,
    limit: u64,
    batch: Option<u32>,
    query_id: Uuid,
) -> Document {
    let mut c = doc! {"find": coll, "filter": filter};
    if !sort.is_empty() {
        c.insert("sort", sort);
    }
    if skip > 0 {
        c.insert("skip", skip as i64);
    }
    if limit > 0 {
        c.insert("limit", limit as i64);
    }
    if let Some(b) = batch {
        c.insert("batchSize", i64::from(b.max(1)));
    }
    c.insert("comment", query_id.to_string());
    c
}

/// Explain a statement.
pub async fn explain_statement(
    cmd: &mut Cmd<'_>,
    conn: &Conn,
    req: &ExplainRequest,
    default_db: &str,
) -> Result<RawPlan> {
    let stmts = mongo_shell::parse(&req.text)?;
    let [stmt] = stmts.as_slice() else {
        return Err(bad("exactly one statement can be explained"));
    };
    let db = stmt
        .database
        .clone()
        .or_else(|| req.database.clone())
        .unwrap_or_else(|| default_db.to_string());
    let planned = plan(stmt, &db, "")?.ok_or_else(|| bad("this statement cannot be explained"))?;
    match planned.kind {
        Kind::Cursor
        | Kind::CountAgg
        | Kind::Count
        | Kind::Distinct
        | Kind::Update
        | Kind::Delete
        | Kind::FindAndModify
            if !planned.cmd.contains_key("listCollections")
                && !planned.cmd.contains_key("listIndexes") => {}
        _ => return Err(Error::Unsupported("explain_this_statement".into())),
    }
    let verbosity = if req.analyze {
        "executionStats"
    } else {
        "queryPlanner"
    };
    let reply = cmd
        .run(
            &planned.db,
            doc! {"explain": planned.cmd, "verbosity": verbosity},
        )
        .await?;
    Ok(RawPlan {
        format: dbstudio_core::PlanFormat::MongoExplain,
        text: doc_to_ejson(&reply, true, true),
        statement: req.text.clone(),
        analyzed: req.analyze,
        rolled_back: false,
        server_version: conn.version.clone(),
        captured_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        extra: Default::default(),
    })
}

#[async_trait]
impl Session for MongoSession {
    async fn ping(&self) -> Result<()> {
        Cmd::pooled(self.client())
            .run("admin", doc! {"ping": 1})
            .await
            .map(|_| ())
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        let mut extra = std::collections::BTreeMap::new();
        if let Some(rs) = &self.conn.set_name {
            extra.insert("replicaSet".into(), rs.clone());
        }
        extra.insert("transactions".into(), self.conn.transactions.to_string());
        extra.insert(
            "tls".into(),
            match (self.conn.tls_active, self.conn.tls_fallback) {
                (true, _) => "true".to_string(),
                (false, true) => "false (fallback)".to_string(),
                (false, false) => "false".to_string(),
            },
        );
        Ok(ServerInfo {
            product: "MongoDB".into(),
            version: self.conn.version.clone(),
            extra,
        })
    }

    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        let started = Instant::now();
        let statements = mongo_shell::parse(&req.text)?;
        let reg = Registered::new(&self.running, req.query_id);
        let mut ctx = run_ctx(req.query_id, req.max_rows, req.batch_size, &reg);
        ctx.read_only = self.conn.read_only();
        let mut db = req
            .database
            .clone()
            .unwrap_or_else(|| self.conn.default_db());
        let fut = async {
            let mut cmd = Cmd::pooled(self.client());
            let (n, _) = run_script(&mut cmd, &ctx, &mut db, &statements, false, sink).await?;
            Ok(ExecSummary {
                statements: n,
                elapsed_ms: started.elapsed().as_millis() as u64,
            })
        };
        with_timeout(
            req.timeout_ms,
            self.client(),
            &self.running,
            &reg,
            req.query_id,
            fut,
        )
        .await
    }

    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        let flag = self.running.lock().unwrap().get(&query_id).cloned();
        let Some(flag) = flag else {
            return Ok(());
        };
        flag.store(true, Ordering::Release);
        kill_op(self.client(), &self.running, query_id).await
    }

    async fn list_children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
        let Some(last) = parent.last() else {
            return Ok(self
                .database_names()
                .await?
                .into_iter()
                .map(|n| ObjectNode {
                    path: Self::db_path(&n),
                    kind: ObjectKind::Database,
                    name: n,
                    has_children: true,
                    detail: None,
                })
                .collect());
        };
        let Some((db, coll)) = resolve(parent) else {
            return Ok(vec![]);
        };
        let mut cmd = Cmd::pooled(self.client());
        match last.kind {
            ObjectKind::Database => {
                let has_views = list_collections(&mut cmd, &db)
                    .await?
                    .iter()
                    .any(|(n, t, _)| t == "view" && self.visible(n));
                let mut out = vec![ObjectNode {
                    path: parent.child(ObjectKind::Folder, folder::COLLECTIONS),
                    kind: ObjectKind::Folder,
                    name: folder::COLLECTIONS.into(),
                    has_children: true,
                    detail: None,
                }];
                if has_views {
                    out.push(ObjectNode {
                        path: parent.child(ObjectKind::Folder, folder::VIEWS),
                        kind: ObjectKind::Folder,
                        name: folder::VIEWS.into(),
                        has_children: true,
                        detail: None,
                    });
                }
                Ok(out)
            }
            ObjectKind::Folder
                if last.name == folder::COLLECTIONS || last.name == folder::VIEWS =>
            {
                let want_view = last.name == folder::VIEWS;
                let items: Vec<_> = list_collections(&mut cmd, &db)
                    .await?
                    .into_iter()
                    .filter(|(n, t, _)| (t == "view") == want_view && self.visible(n))
                    .collect();
                let names: Vec<String> = items.iter().map(|i| i.0.clone()).collect();
                let counts = if want_view {
                    HashMap::new()
                } else {
                    self.collection_counts(&db, &names).await
                };
                Ok(items
                    .into_iter()
                    .map(|(n, _, _)| ObjectNode {
                        path: parent.child(
                            if want_view {
                                ObjectKind::View
                            } else {
                                ObjectKind::Collection
                            },
                            n.clone(),
                        ),
                        kind: if want_view {
                            ObjectKind::View
                        } else {
                            ObjectKind::Collection
                        },
                        has_children: !want_view,
                        detail: counts.get(&n).map(|c| human_count(*c)),
                        name: n,
                    })
                    .collect())
            }
            ObjectKind::Collection => Ok(vec![ObjectNode {
                path: parent.child(ObjectKind::Folder, folder::INDEXES),
                kind: ObjectKind::Folder,
                name: folder::INDEXES.into(),
                has_children: true,
                detail: None,
            }]),
            ObjectKind::Folder if last.name == folder::INDEXES => {
                let Some((coll, _)) = coll else {
                    return Ok(vec![]);
                };
                Ok(list_indexes(&mut cmd, &db, &coll)
                    .await?
                    .into_iter()
                    .filter_map(|spec| {
                        let name = spec.get_str("name").ok()?.to_string();
                        let mut detail = index_columns(&spec).join(", ");
                        if spec.get_bool("unique").unwrap_or(false) {
                            detail.push_str(" (unique)");
                        }
                        Some(ObjectNode {
                            path: parent.child(ObjectKind::Index, name.clone()),
                            kind: ObjectKind::Index,
                            name,
                            has_children: false,
                            detail: Some(detail),
                        })
                    })
                    .collect())
            }
            _ => Ok(vec![]),
        }
    }

    async fn object_ddl(&self, path: &ObjectPath) -> Result<String> {
        let (db, coll) = resolve(path).ok_or_else(|| Error::NotFound("database".into()))?;
        let Some((coll, _)) = coll else {
            return Err(Error::Unsupported("object_ddl".into()));
        };
        let items = self
            .ddl_items(&db, Some(&coll), &Default::default())
            .await?;
        if items.is_empty() {
            return Err(Error::NotFound(coll));
        }
        Ok(items
            .into_iter()
            .map(|i| i.sql)
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn table_details(&self, path: &ObjectPath) -> Result<TableDetails> {
        let (db, coll) = resolve(path).ok_or_else(|| Error::NotFound("database".into()))?;
        let Some((coll, kind)) = coll else {
            return Err(Error::Unsupported("table_details".into()));
        };
        let mut cmd = Cmd::pooled(self.client());
        let docs = sample(&mut cmd, &db, &coll, 100).await?;
        let columns = infer_columns(&docs);
        let indexes = list_indexes(&mut cmd, &db, &coll)
            .await?
            .into_iter()
            .map(|s| IndexDetail {
                name: s.get_str("name").unwrap_or("").to_string(),
                columns: index_columns(&s),
                unique: s.get_bool("unique").unwrap_or(false)
                    || s.get_str("name").ok() == Some("_id_"),
                primary: s.get_str("name").ok() == Some("_id_"),
                method: None,
                definition: Some(doc_to_ejson(&s, false, false)),
                partial: false,
            })
            .collect();
        let estimated = cmd
            .run(&db, doc! {"count": &coll})
            .await
            .ok()
            .map(|d| num(&d, "n").max(0) as u64);
        Ok(TableDetails {
            path: path.clone(),
            kind: if kind == ObjectKind::View {
                ObjectKind::View
            } else {
                ObjectKind::Collection
            },
            comment: None,
            columns,
            indexes,
            foreign_keys: vec![],
            estimated_rows: estimated,
            size_bytes: None,
        })
    }

    async fn fetch_table(
        &self,
        req: TableDataRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let (db, coll) = resolve(&req.table).ok_or_else(|| Error::NotFound("database".into()))?;
        let (coll, _) = coll.ok_or_else(|| Error::NotFound("collection".into()))?;
        let filter = build_filter(&req.filters, req.raw_where.as_deref())?;
        let sort = build_sort(&req.sort, true);
        let command = find_command(
            &coll,
            filter,
            sort,
            req.offset,
            req.limit,
            req.batch_size,
            req.query_id,
        );
        self.find_stream(req.query_id, &db, command, sink, req.batch_size)
            .await
    }

    async fn count_table(&self, req: TableCountRequest) -> Result<u64> {
        let (db, coll) = resolve(&req.table).ok_or_else(|| Error::NotFound("database".into()))?;
        let (coll, kind) = coll.ok_or_else(|| Error::NotFound("collection".into()))?;
        let filter = build_filter(&req.filters, req.raw_where.as_deref())?;
        let id = req.query_id.unwrap_or_else(Uuid::new_v4);
        let reg = Registered::new(&self.running, id);
        let fut = async {
            let mut cmd = Cmd::pooled(self.client());
            if filter.is_empty() && kind != ObjectKind::View {
                let r = cmd
                    .run(&db, doc! {"count": &coll, "comment": id.to_string()})
                    .await?;
                return Ok(num(&r, "n").max(0) as u64);
            }
            let mut pipeline = vec![];
            if !filter.is_empty() {
                pipeline.push(Bson::Document(doc! {"$match": filter}));
            }
            pipeline.push(Bson::Document(
                doc! {"$group": {"_id": 1, "n": {"$sum": 1}}},
            ));
            let r = cmd
                .run(
                    &db,
                    doc! {"aggregate": &coll, "pipeline": pipeline, "cursor": {}, "comment": id.to_string()},
                )
                .await?;
            let p = pending_from_reply(&r, &db)?;
            Ok(p.buffer.front().map(|d| num(d, "n")).unwrap_or(0).max(0) as u64)
        };
        with_timeout(None, self.client(), &self.running, &reg, id, fut).await
    }

    async fn search_objects(&self, req: ObjectSearchRequest) -> Result<Vec<ObjectNode>> {
        let needle = req.query.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(vec![]);
        }
        let allowed = |k: ObjectKind| {
            req.kinds.is_empty()
                || req.kinds.contains(&k)
                || (k == ObjectKind::Collection && req.kinds.contains(&ObjectKind::Table))
        };
        let dbs = match resolve(&req.scope) {
            Some((db, _)) => vec![db],
            None => self.database_names().await?,
        };
        let mut cmd = Cmd::pooled(self.client());
        let mut hits: Vec<(u8, ObjectNode)> = vec![];
        for db in dbs {
            for (name, ty, _) in list_collections(&mut cmd, &db).await? {
                if !self.visible(&name) {
                    continue;
                }
                let lower = name.to_lowercase();
                if !lower.contains(&needle) {
                    continue;
                }
                let is_view = ty == "view";
                let kind = if is_view {
                    ObjectKind::View
                } else {
                    ObjectKind::Collection
                };
                if !allowed(kind) && !(is_view && req.kinds.is_empty()) {
                    continue;
                }
                let rank = if lower == needle {
                    0
                } else if lower.starts_with(&needle) {
                    1
                } else {
                    2
                };
                hits.push((
                    rank,
                    ObjectNode {
                        path: Self::db_path(&db)
                            .child(
                                ObjectKind::Folder,
                                if is_view {
                                    folder::VIEWS
                                } else {
                                    folder::COLLECTIONS
                                },
                            )
                            .child(kind, name.clone()),
                        kind,
                        name,
                        has_children: !is_view,
                        detail: Some(db.clone()),
                    },
                ));
            }
        }
        hits.sort_by(|a, b| (a.0, &a.1.name, &a.1.detail).cmp(&(b.0, &b.1.name, &b.1.detail)));
        let limit = req.limit.clamp(1, 1000) as usize;
        Ok(hits.into_iter().take(limit).map(|h| h.1).collect())
    }

    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect::MongoShell
    }

    async fn open_lease(&self, opts: LeaseOptions) -> Result<Box<dyn Lease>> {
        Ok(Box::new(
            crate::lease::MongoLease::open(self.conn.clone(), opts).await?,
        ))
    }

    async fn editability(&self, path: &ObjectPath) -> Result<Editability> {
        let kind = path.last().map(|s| s.kind);
        let not = |reason| Editability {
            editable: false,
            key_columns: vec![],
            read_only_columns: vec![],
            reason: Some(reason),
            atomic: self.conn.transactions,
        };
        if kind == Some(ObjectKind::View) {
            return Ok(not(NotEditableReason::NotATable));
        }
        if resolve(path).and_then(|r| r.1).is_none() {
            return Ok(not(NotEditableReason::NotATable));
        }
        if self.conn.read_only() {
            return Ok(not(NotEditableReason::ReadOnlyConnection));
        }
        Ok(Editability {
            editable: true,
            key_columns: vec!["_id".into()],
            read_only_columns: vec!["_id".into()],
            reason: None,
            atomic: self.conn.transactions,
        })
    }

    async fn preview_changes(&self, changes: &ChangeSet) -> Result<Vec<String>> {
        crate::edit::preview(changes)
    }

    async fn apply_changes(&self, changes: ChangeSet) -> Result<ApplyResult> {
        crate::edit::apply(&self.conn, changes).await
    }

    async fn explain(&self, req: ExplainRequest) -> Result<RawPlan> {
        let reg = Registered::new(&self.running, req.query_id);
        let fut = async {
            let mut cmd = Cmd::pooled(self.client());
            explain_statement(&mut cmd, &self.conn, &req, &self.conn.default_db()).await
        };
        with_timeout(
            req.timeout_ms,
            self.client(),
            &self.running,
            &reg,
            req.query_id,
            fut,
        )
        .await
    }

    async fn schema_model(&self, req: SchemaModelRequest) -> Result<SchemaModel> {
        let (db, _) = resolve(&req.scope).ok_or_else(|| Error::NotFound("database".into()))?;
        let size = req.sample_size.unwrap_or(1000).clamp(1, 20_000);
        let mut cmd = Cmd::pooled(self.client());
        let wanted: Vec<String> = req
            .tables
            .iter()
            .filter_map(|p| resolve(p).and_then(|r| r.1).map(|c| c.0))
            .collect();
        let mut listed: Vec<_> = list_collections(&mut cmd, &db)
            .await?
            .into_iter()
            .filter(|(n, t, _)| {
                self.visible(n)
                    && (t != "view" || req.include_views)
                    && (wanted.is_empty() || wanted.contains(n))
            })
            .collect();
        let truncated = listed.len() > MAX_TABLES;
        listed.truncate(MAX_TABLES);
        let mut tables = Vec::new();
        let mut for_relations = Vec::new();
        for (name, ty, _) in listed {
            let is_view = ty == "view";
            let kind = if is_view {
                ObjectKind::View
            } else {
                ObjectKind::Collection
            };
            let path = Self::db_path(&db)
                .child(
                    ObjectKind::Folder,
                    if is_view {
                        folder::VIEWS
                    } else {
                        folder::COLLECTIONS
                    },
                )
                .child(kind, name.clone());
            let docs = sample(&mut cmd, &db, &name, size).await?;
            let mut columns = infer_columns(&docs);
            if req.detail == dbstudio_core::ModelDetail::Keys {
                columns.retain(|c| c.primary_key || c.type_name.contains("objectId"));
            }
            let mut unique_keys = vec![];
            if !is_view {
                for s in list_indexes(&mut cmd, &db, &name).await? {
                    if s.get_bool("unique").unwrap_or(false)
                        && s.get_str("name").ok() != Some("_id_")
                    {
                        unique_keys.push(index_columns(&s));
                    }
                }
            }
            let estimated = cmd
                .run(&db, doc! {"count": &name})
                .await
                .ok()
                .map(|d| num(&d, "n").max(0) as u64);
            for_relations.push((path.clone(), name.clone(), columns.clone()));
            tables.push(TableModel {
                path,
                kind,
                schema: None,
                name,
                comment: None,
                columns,
                primary_key: vec!["_id".into()],
                unique_keys,
                estimated_rows: estimated,
            });
        }
        Ok(SchemaModel {
            scope: req.scope,
            tables,
            relations: infer_relations(&for_relations),
            inferred: true,
            truncated,
        })
    }

    async fn generate_ddl(&self, req: DdlRequest) -> Result<Vec<DdlItem>> {
        let (db, coll) = resolve(&req.scope).ok_or_else(|| Error::NotFound("database".into()))?;
        let objects: Vec<String> = req
            .objects
            .iter()
            .filter_map(|p| resolve(p).and_then(|r| r.1).map(|c| c.0))
            .collect();
        let only = match coll {
            Some((c, _)) => Some(c),
            None if objects.len() == 1 => Some(objects[0].clone()),
            None => None,
        };
        let mut items = self.ddl_items(&db, only.as_deref(), &req.options).await?;
        if only.is_none() && !objects.is_empty() {
            items.retain(|i| i.path.last().is_some_and(|s| objects.contains(&s.name)));
        }
        Ok(items)
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

impl MongoSession {
    fn visible(&self, name: &str) -> bool {
        self.conn.show_system() || !name.starts_with("system.")
    }

    async fn ddl_items(
        &self,
        db: &str,
        only: Option<&str>,
        opts: &dbstudio_core::DdlOptions,
    ) -> Result<Vec<DdlItem>> {
        let mut cmd = Cmd::pooled(self.client());
        let mut pre = vec![];
        let mut post = vec![];
        for (name, ty, options) in list_collections(&mut cmd, db).await? {
            if !self.visible(&name) || only.is_some_and(|o| o != name) {
                continue;
            }
            if ty == "view" && !opts.include_views {
                continue;
            }
            let parts = ddl_for(self.client(), db, &name, &ty, &options, opts).await?;
            for (kind, section, text) in parts {
                let item = ddl_item(db, &name, kind, section, text);
                if section == dbstudio_core::DdlSection::PreData {
                    pre.push(item);
                } else {
                    post.push(item);
                }
            }
        }
        // Views depend on collections: create them last within PreData.
        pre.sort_by_key(|i| i.kind == ObjectKind::View);
        pre.extend(post);
        Ok(pre)
    }
}
