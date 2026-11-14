//! Shared helpers of the export / import tests: a scripted in-memory session and a reporter.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dbstudio_core::{
    CancelToken, ColumnDetail, ColumnMeta, DbKind, DdlItem, DdlLanguage, DdlRequest, DdlSection,
    EndOutcome, Error, ExecSummary, JobProgress, JobReporter, ObjectKind, ObjectNode, ObjectPath,
    QueryRequest, Result, ResultSink, SchemaModel, SchemaModelRequest, ScriptDialect, ServerInfo,
    Session, StatementEnd, TableDetails, TableModel, Value, ValueKind,
};
use dbstudio_export::ExportContext;

/// The statement names `name` quoted the way PostgreSQL (`"x"`) or MySQL / ClickHouse (`` `x` ``)
/// quote identifiers.
fn quoted_in(text: &str, name: &str) -> bool {
    text.contains(&format!("\"{name}\"")) || text.contains(&format!("`{name}`"))
}

pub fn table_path(name: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Database, "app")
        .child(ObjectKind::Schema, "public")
        .child(ObjectKind::Folder, "tables")
        .child(ObjectKind::Table, name)
}

pub fn schema_path() -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Database, "app")
        .child(ObjectKind::Schema, "public")
}

pub fn column(name: &str, ordinal: u32, ty: &str, kind: ValueKind, pk: bool) -> ColumnDetail {
    ColumnDetail {
        name: name.into(),
        ordinal,
        type_name: ty.into(),
        value_kind: kind,
        nullable: !pk,
        default: None,
        comment: if name == "name" {
            Some("display name".into())
        } else {
            None
        },
        primary_key: pk,
        extra: None,
    }
}

/// `orders(id bigint pk, name text, price numeric, note bytea)`-like table model.
pub fn model(name: &str, rows: u64) -> TableModel {
    TableModel {
        path: table_path(name),
        kind: ObjectKind::Table,
        schema: Some("public".into()),
        name: name.into(),
        comment: None,
        columns: vec![
            column("id", 1, "bigint", ValueKind::Int, true),
            column("name", 2, "text", ValueKind::Text, false),
            column("price", 3, "numeric", ValueKind::Decimal, false),
        ],
        primary_key: vec!["id".into()],
        unique_keys: vec![],
        estimated_rows: Some(rows),
    }
}

pub fn rows_of(n: usize) -> Vec<Vec<Value>> {
    (1..=n)
        .map(|i| {
            vec![
                Value::Int(i as i64),
                if i % 4 == 0 {
                    Value::Null
                } else {
                    Value::Text(format!("name {i}, \"quoted\""))
                },
                Value::Decimal(format!("{i}.50")),
            ]
        })
        .collect()
}

/// Session that serves canned tables and logs every statement it is asked to run.
pub struct FakeSession {
    pub tables: Vec<TableModel>,
    pub data: HashMap<String, Vec<Vec<Value>>>,
    pub log: Mutex<Vec<String>>,
    /// An `INSERT` containing this text fails with a query error.
    pub fail_on: Mutex<Option<String>>,
    pub batch: usize,
    /// Cancels the token after this many rows were delivered (cancellation tests).
    pub cancel_after: Mutex<Option<(usize, CancelToken)>>,
    /// `open_lease` hands out a lease whose transaction cannot start.
    pub lease_begin_fails: Mutex<bool>,
}

impl FakeSession {
    pub fn with_tables(tables: &[(&str, usize)]) -> Arc<FakeSession> {
        let mut models = Vec::new();
        let mut data = HashMap::new();
        for (name, n) in tables {
            models.push(model(name, *n as u64));
            data.insert((*name).to_string(), rows_of(*n));
        }
        Arc::new(FakeSession {
            tables: models,
            data,
            log: Mutex::new(Vec::new()),
            fail_on: Mutex::new(None),
            batch: 3,
            cancel_after: Mutex::new(None),
            lease_begin_fails: Mutex::new(false),
        })
    }

    pub fn statements(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn columns_of(&self, table: &str) -> Vec<ColumnMeta> {
        self.tables
            .iter()
            .find(|t| t.name == table)
            .map(|t| {
                t.columns
                    .iter()
                    .map(|c| ColumnMeta {
                        name: c.name.clone(),
                        type_name: c.type_name.clone(),
                        value_kind: c.value_kind,
                        nullable: Some(c.nullable),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[async_trait]
impl Session for FakeSession {
    async fn ping(&self) -> Result<()> {
        Ok(())
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        Ok(ServerInfo::default())
    }

    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        let text = req.text.clone();
        self.log.lock().unwrap().push(text.clone());
        let upper = text.trim_start().to_uppercase();
        let canned = |sink: &mut dyn ResultSink,
                      cols: &[(&str, ValueKind)],
                      rows: Vec<Vec<Value>>|
         -> Result<()> {
            sink.on_columns(
                cols.iter()
                    .map(|(n, k)| ColumnMeta {
                        name: (*n).into(),
                        type_name: "x".into(),
                        value_kind: *k,
                        nullable: Some(true),
                    })
                    .collect(),
            )?;
            let n = rows.len() as u64;
            sink.on_rows(rows)?;
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Rows {
                    row_count: n,
                    truncated: false,
                },
                elapsed_ms: 0,
            })
        };
        if upper.starts_with("SELECT 1 AS DUP") {
            canned(
                sink,
                &[
                    ("a", ValueKind::Int),
                    ("a", ValueKind::Int),
                    ("a#2", ValueKind::Int),
                ],
                vec![vec![Value::Int(1), Value::Int(2), Value::Int(3)]],
            )?;
        } else if upper.starts_with("SELECT 1 AS FORMULA") {
            canned(
                sink,
                &[("t", ValueKind::Text), ("n", ValueKind::Int)],
                vec![
                    vec![Value::Text("=1+1".into()), Value::Int(-5)],
                    vec![Value::Text("plain".into()), Value::Int(7)],
                ],
            )?;
        } else if upper.starts_with("SELECT 1 AS BIG") {
            canned(
                sink,
                &[("n", ValueKind::Int)],
                vec![
                    vec![Value::Int(1_000_000_000_000_000)],
                    vec![Value::Int(999_999_999_999_999)],
                ],
            )?;
        } else if upper.starts_with("SELECT 1 AS HOSTILE") {
            canned(
                sink,
                &[("t", ValueKind::Text), ("ts", ValueKind::Timestamp)],
                vec![vec![
                    Value::Text("\\', 0); DROP TABLE users; -- ".into()),
                    Value::Timestamp("2024-05-06 07:08:09+00".into()),
                ]],
            )?;
        } else if upper.starts_with("SELECT 1 AS") {
            sink.on_columns(vec![ColumnMeta {
                name: "n".into(),
                type_name: "int4".into(),
                value_kind: ValueKind::Int,
                nullable: Some(false),
            }])?;
            sink.on_rows(vec![vec![Value::Int(1)], vec![Value::Int(2)]])?;
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Rows {
                    row_count: 2,
                    truncated: false,
                },
                elapsed_ms: 0,
            })?;
        } else if upper.starts_with("SELECT")
            && self.tables.iter().any(|t| quoted_in(&text, &t.name))
        {
            let table = self
                .tables
                .iter()
                .find(|t| quoted_in(&text, &t.name))
                .map(|t| t.name.clone())
                .expect("checked above");
            sink.on_columns(self.columns_of(&table))?;
            let rows = self.data[&table].clone();
            let mut sent = 0usize;
            for chunk in rows.chunks(self.batch) {
                sink.on_rows(chunk.to_vec())?;
                sent += chunk.len();
                let cancel = self.cancel_after.lock().unwrap().clone();
                if let Some((n, token)) = cancel {
                    if sent >= n {
                        token.cancel();
                    }
                }
            }
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Rows {
                    row_count: sent as u64,
                    truncated: false,
                },
                elapsed_ms: 0,
            })?;
        } else if upper.starts_with("INSERT") {
            let fail = self.fail_on.lock().unwrap().clone();
            if let Some(f) = fail {
                if text.contains(&f) {
                    return Err(Error::Query {
                        message: format!("insert refused: {f}"),
                        position: None,
                    });
                }
            }
            let tuples = text.lines().filter(|l| l.starts_with('(')).count() as u64;
            let n = tuples.max(1);
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Affected { rows: n },
                elapsed_ms: 0,
            })?;
        } else {
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Done,
                elapsed_ms: 0,
            })?;
        }
        Ok(ExecSummary::default())
    }

    async fn open_lease(
        &self,
        _opts: dbstudio_core::LeaseOptions,
    ) -> Result<Box<dyn dbstudio_core::Lease>> {
        if *self.lease_begin_fails.lock().unwrap() {
            return Ok(Box::new(BeginFails));
        }
        Err(Error::Unsupported("leases".into()))
    }

    async fn list_children(&self, _p: &ObjectPath) -> Result<Vec<ObjectNode>> {
        Ok(vec![])
    }

    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect::Postgres
    }

    async fn table_details(&self, path: &ObjectPath) -> Result<TableDetails> {
        let name = path.last().map(|s| s.name.clone()).unwrap_or_default();
        let t = self
            .tables
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| Error::NotFound(name.clone()))?;
        Ok(TableDetails {
            path: path.clone(),
            kind: ObjectKind::Table,
            comment: None,
            columns: t.columns.clone(),
            indexes: vec![],
            foreign_keys: vec![],
            estimated_rows: t.estimated_rows,
            size_bytes: None,
        })
    }

    async fn schema_model(&self, req: SchemaModelRequest) -> Result<SchemaModel> {
        let tables: Vec<TableModel> = if req.tables.is_empty() {
            self.tables.clone()
        } else {
            self.tables
                .iter()
                .filter(|t| req.tables.contains(&t.path))
                .cloned()
                .collect()
        };
        Ok(SchemaModel {
            scope: req.scope,
            tables,
            relations: vec![],
            inferred: false,
            truncated: false,
        })
    }

    async fn generate_ddl(&self, req: DdlRequest) -> Result<Vec<DdlItem>> {
        let mut items = Vec::new();
        for t in &self.tables {
            if !req.objects.is_empty() && !req.objects.contains(&t.path) {
                continue;
            }
            items.push(DdlItem {
                path: t.path.clone(),
                kind: ObjectKind::Table,
                section: DdlSection::PreData,
                sql: format!(
                    "CREATE TABLE \"public\".\"{}\" (id bigint PRIMARY KEY);",
                    t.name
                ),
                language: DdlLanguage::Sql,
                depends_on: vec![],
            });
            items.push(DdlItem {
                path: t.path.clone(),
                kind: ObjectKind::Index,
                section: DdlSection::PostData,
                sql: format!(
                    "CREATE INDEX \"{0}_idx\" ON \"public\".\"{0}\" (name);",
                    t.name
                ),
                language: DdlLanguage::Sql,
                depends_on: vec![],
            });
        }
        Ok(items)
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// Reporter that records progress and carries the cancel token.
pub struct Recorder {
    pub token: CancelToken,
    pub seen: Mutex<Vec<JobProgress>>,
}

impl Recorder {
    pub fn new() -> Arc<Recorder> {
        Arc::new(Recorder {
            token: CancelToken::default(),
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl JobReporter for Recorder {
    fn progress(&self, p: &JobProgress) {
        self.seen.lock().unwrap().push(p.clone());
    }
    fn cancel_token(&self) -> &CancelToken {
        &self.token
    }
}

pub fn context(session: Arc<dyn Session>, reporter: Arc<Recorder>) -> ExportContext {
    ExportContext {
        session,
        kind: DbKind::Postgres,
        dialect: ScriptDialect::Postgres,
        reporter,
    }
}

/// A lease that works, except that it cannot start a transaction.
pub struct BeginFails;

#[async_trait]
impl dbstudio_core::Lease for BeginFails {
    async fn execute(&self, _r: QueryRequest, _s: &mut dyn ResultSink) -> Result<ExecSummary> {
        Ok(ExecSummary::default())
    }
    async fn fetch_more(
        &self,
        _q: uuid::Uuid,
        _m: u64,
        _b: Option<u32>,
        _s: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        Err(Error::Unsupported("x".into()))
    }
    async fn discard_result(&self) -> Result<()> {
        Ok(())
    }
    async fn cancel(&self, _q: uuid::Uuid) -> Result<()> {
        Ok(())
    }
    async fn begin(&self, _o: dbstudio_core::TxOptions) -> Result<()> {
        Err(Error::query("cannot start a transaction"))
    }
    async fn commit(&self) -> Result<()> {
        Ok(())
    }
    async fn rollback(&self) -> Result<()> {
        Ok(())
    }
    async fn set_auto_commit(&self, _on: bool) -> Result<()> {
        Ok(())
    }
    async fn set_context(&self, _c: dbstudio_core::LeaseContext) -> Result<()> {
        Ok(())
    }
    fn state(&self) -> dbstudio_core::LeaseState {
        dbstudio_core::LeaseState::initial(&dbstudio_core::LeaseOptions::default())
    }
    async fn ping(&self) -> Result<()> {
        Ok(())
    }
    async fn close(&self) -> Result<dbstudio_core::LeaseCloseReport> {
        Ok(dbstudio_core::LeaseCloseReport {
            rolled_back: false,
            forced: false,
        })
    }
}
