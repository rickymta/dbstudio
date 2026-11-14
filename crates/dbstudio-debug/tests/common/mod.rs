//! Fakes shared by the engine tests: a scripted lease, a guard and a recording sink.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dbstudio_core::debug::{
    DebugOptions, DebugOutput, DebugOutputEvent, DebugStartRequest, DebugState, DebugStatus,
    PauseReason, TerminateReason,
};
use dbstudio_core::{
    Capabilities, ColumnMeta, ConfirmationGrant, ConfirmationRequest, DbKind, EndOutcome, Error,
    ExecSummary, Lease, LeaseCloseReport, LeaseContext, LeaseState, QueryRequest, Result,
    ResultSink, ScriptDialect, StatementClass, StatementEnd, TxOptions, TxStatus, Value, ValueKind,
};
use dbstudio_debug::actor::{ActorHandle, start};
use dbstudio_debug::{
    DebugSettings, DebugSink, DialectProfile, GateRequest, GatedText, ProxyOpener, RoutineDebugger,
    StepGate, StepRecord,
};
use tokio::sync::Notify;
use uuid::Uuid;

/// What a statement matching a rule answers.
#[derive(Clone)]
pub enum Reply {
    Rows(Vec<&'static str>, Vec<Vec<Value>>),
    Affected(u64),
    Done,
    Fail(String),
    /// Blocks until released (or cancelled).
    Block,
    Messages(Vec<&'static str>),
}

struct Rule {
    needle: String,
    replies: VecDeque<Reply>,
}

pub struct FakeLease {
    state: Mutex<LeaseState>,
    rules: Mutex<Vec<Rule>>,
    pub log: Mutex<Vec<String>>,
    release: Notify,
    cancelled: Notify,
    pub dialect: Mutex<Option<ScriptDialect>>,
    fail_begin: std::sync::atomic::AtomicBool,
}

impl FakeLease {
    pub fn new(auto_commit: bool) -> Arc<FakeLease> {
        Arc::new(FakeLease {
            state: Mutex::new(LeaseState {
                auto_commit,
                transaction: TxStatus::Idle,
                running: None,
                pending_result: None,
                database: Some("app".into()),
                schema: None,
                broken: false,
            }),
            rules: Mutex::new(vec![]),
            log: Mutex::new(vec![]),
            release: Notify::new(),
            cancelled: Notify::new(),
            dialect: Mutex::new(None),
            fail_begin: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// `begin` fails from now on.
    pub fn fail_begin(&self) {
        self.fail_begin
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Statements containing `needle` (case-insensitive) answer `reply`; several replies are
    /// consumed in order, the last one repeats.
    pub fn on(&self, needle: &str, replies: Vec<Reply>) {
        self.rules.lock().unwrap().push(Rule {
            needle: needle.to_lowercase(),
            replies: replies.into(),
        });
    }

    pub fn release(&self) {
        self.release.notify_one();
    }

    pub fn set_tx(&self, tx: TxStatus) {
        self.state.lock().unwrap().transaction = tx;
    }

    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    pub fn ran(&self, needle: &str) -> usize {
        self.log().iter().filter(|t| t.contains(needle)).count()
    }

    pub fn break_it(&self) {
        self.state.lock().unwrap().broken = true;
    }

    fn pick(&self, text: &str) -> Reply {
        let lower = text.to_lowercase();
        let mut rules = self.rules.lock().unwrap();
        for r in rules.iter_mut() {
            if lower.contains(&r.needle) {
                return if r.replies.len() > 1 {
                    r.replies.pop_front().unwrap()
                } else {
                    r.replies.front().cloned().unwrap_or(Reply::Done)
                };
            }
        }
        Reply::Done
    }
}

pub fn int(i: i64) -> Value {
    Value::Int(i)
}

#[async_trait]
impl Lease for FakeLease {
    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        self.log.lock().unwrap().push(req.text.clone());
        let reply = self.pick(&req.text);
        let end = |outcome| StatementEnd {
            outcome,
            elapsed_ms: 1,
        };
        match reply {
            Reply::Rows(cols, rows) => {
                sink.on_columns(
                    cols.iter()
                        .map(|c| ColumnMeta {
                            name: (*c).into(),
                            type_name: "int8".into(),
                            value_kind: ValueKind::Int,
                            nullable: None,
                        })
                        .collect(),
                )?;
                let n = rows.len() as u64;
                sink.on_rows(rows)?;
                sink.on_statement_end(end(EndOutcome::Rows {
                    row_count: n,
                    truncated: false,
                }))?;
            }
            Reply::Affected(n) => sink.on_statement_end(end(EndOutcome::Affected { rows: n }))?,
            Reply::Done => sink.on_statement_end(end(EndOutcome::Done))?,
            Reply::Fail(m) => {
                if m == "readonly" {
                    return Err(Error::ReadOnlyViolation {
                        message: "no".into(),
                        position: None,
                    });
                }
                self.state.lock().unwrap().transaction =
                    if self.state.lock().unwrap().transaction == TxStatus::Active {
                        TxStatus::Failed
                    } else {
                        TxStatus::Idle
                    };
                return Err(Error::query(m));
            }
            Reply::Block => {
                tokio::select! {
                    () = self.release.notified() => {}
                    () = self.cancelled.notified() => return Err(Error::Cancelled),
                }
                sink.on_statement_end(end(EndOutcome::Done))?;
            }
            Reply::Messages(ms) => {
                for m in ms {
                    sink.on_message(m.into())?;
                }
                sink.on_statement_end(end(EndOutcome::Done))?;
            }
        }
        Ok(ExecSummary {
            statements: 1,
            elapsed_ms: 1,
        })
    }

    async fn fetch_more(
        &self,
        _q: Uuid,
        _m: u64,
        _b: Option<u32>,
        _s: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        Err(Error::NotFound("none".into()))
    }

    async fn discard_result(&self) -> Result<()> {
        Ok(())
    }

    async fn cancel(&self, _q: Uuid) -> Result<()> {
        self.cancelled.notify_one();
        Ok(())
    }

    async fn begin(&self, _o: TxOptions) -> Result<()> {
        self.log.lock().unwrap().push("<begin>".into());
        if self.fail_begin.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::query("cannot begin"));
        }
        self.state.lock().unwrap().transaction = TxStatus::Active;
        Ok(())
    }

    async fn commit(&self) -> Result<()> {
        self.log.lock().unwrap().push("<commit>".into());
        self.state.lock().unwrap().transaction = TxStatus::Idle;
        Ok(())
    }

    async fn rollback(&self) -> Result<()> {
        self.log.lock().unwrap().push("<rollback>".into());
        self.state.lock().unwrap().transaction = TxStatus::Idle;
        Ok(())
    }

    async fn set_auto_commit(&self, _on: bool) -> Result<()> {
        Ok(())
    }

    async fn set_context(&self, _c: LeaseContext) -> Result<()> {
        Ok(())
    }

    fn state(&self) -> LeaseState {
        self.state.lock().unwrap().clone()
    }

    fn script_dialect(&self) -> Option<ScriptDialect> {
        *self.dialect.lock().unwrap()
    }

    async fn ping(&self) -> Result<()> {
        Ok(())
    }

    async fn close(&self) -> Result<LeaseCloseReport> {
        Ok(LeaseCloseReport {
            rolled_back: false,
            forced: false,
        })
    }
}

/// Guard fake: read-only and production switches on top of the real analyzer.
#[derive(Default)]
pub struct FakeGate {
    pub read_only: Mutex<bool>,
    pub production: Mutex<bool>,
    pub records: Mutex<Vec<StepRecord>>,
    pub params_seen: Mutex<Vec<usize>>,
}

pub const TOKEN: &str = "tok";

impl StepGate for FakeGate {
    fn check(&self, req: GateRequest<'_>) -> Result<GatedText> {
        let analysis = dbstudio_sql::analyze(req.dialect, req.text);
        self.params_seen.lock().unwrap().push(req.params.len());
        if *self.read_only.lock().unwrap()
            && analysis.statements.iter().any(|s| {
                !matches!(
                    s.class,
                    StatementClass::Read
                        | StatementClass::Session
                        | StatementClass::Transaction { .. }
                        | StatementClass::Explain { .. }
                )
            })
        {
            return Err(Error::ReadOnlyViolation {
                message: "read-only connection".into(),
                position: None,
            });
        }
        if *self.production.lock().unwrap()
            && analysis.statements.iter().any(|s| s.destructive.is_some())
            && req.confirm.map(|c| c.token.as_str()) != Some(TOKEN)
        {
            return Err(Error::ConfirmationRequired(Box::new(ConfirmationRequest {
                token: TOKEN.into(),
                confirm_text: "app".into(),
                statements: vec![],
                expires_in_ms: 60_000,
            })));
        }
        let bound = dbstudio_sql::bind(req.dialect, req.text, req.params)?;
        Ok(GatedText {
            text: bound.text,
            map: bound.map,
            analysis,
        })
    }

    fn check_read_only(&self, dialect: ScriptDialect, text: &str) -> Result<GatedText> {
        let analysis = dbstudio_sql::analyze(dialect, text);
        let ok = analysis.statements.len() == 1
            && matches!(
                analysis.statements[0].class,
                StatementClass::Read | StatementClass::Explain { writes: false, .. }
            );
        if !ok {
            return Err(Error::ReadOnlyViolation {
                message: "only a read statement can be evaluated".into(),
                position: None,
            });
        }
        Ok(GatedText {
            text: text.to_string(),
            map: dbstudio_sql::OffsetMap::identity(),
            analysis,
        })
    }

    fn record(&self, step: &StepRecord) {
        self.records.lock().unwrap().push(step.clone());
    }
}

#[derive(Default)]
pub struct RecSink {
    pub states: Mutex<Vec<DebugState>>,
    pub outputs: Mutex<Vec<DebugOutputEvent>>,
}

impl DebugSink for RecSink {
    fn state(&self, state: &DebugState) {
        self.states.lock().unwrap().push(state.clone());
    }

    fn output(&self, event: DebugOutputEvent) {
        self.outputs.lock().unwrap().push(event);
    }
}

impl RecSink {
    pub fn outputs(&self) -> Vec<DebugOutput> {
        self.outputs
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.output.clone())
            .collect()
    }

    pub fn messages(&self) -> Vec<(String, dbstudio_core::debug::MessageSource)> {
        self.outputs()
            .into_iter()
            .filter_map(|o| match o {
                DebugOutput::Message { text, source } => Some((text, source)),
                _ => None,
            })
            .collect()
    }
}

pub struct NoProxy;

#[async_trait]
impl ProxyOpener for NoProxy {
    async fn open_proxy(&self, _db: Option<String>) -> Result<Arc<dyn Lease>> {
        Err(Error::Unsupported("proxy".into()))
    }
}

pub struct Rig {
    pub h: ActorHandle,
    pub lease: Arc<FakeLease>,
    pub gate: Arc<FakeGate>,
    pub sink: Arc<RecSink>,
}

pub fn profile(dialect: ScriptDialect) -> DialectProfile {
    let kind = match dialect {
        ScriptDialect::Postgres => DbKind::Postgres,
        ScriptDialect::MySql { .. } => DbKind::MySql,
        ScriptDialect::MsSql => DbKind::SqlServer,
        ScriptDialect::Oracle => DbKind::Oracle,
        ScriptDialect::MongoShell => DbKind::MongoDb,
        ScriptDialect::RedisCli => DbKind::Redis,
        ScriptDialect::Cql => DbKind::Cassandra,
        ScriptDialect::ClickHouse => DbKind::ClickHouse,
        ScriptDialect::Opaque => DbKind::Postgres,
    };
    DialectProfile {
        kind,
        flavor: None,
        version: None,
        dialect,
        caps: Capabilities {
            manual_transactions: true,
            routine_debug: true,
            ..Capabilities::default()
        },
    }
}

pub fn request(text: &str) -> DebugStartRequest {
    DebugStartRequest {
        lease_id: Uuid::new_v4(),
        text: text.into(),
        range: None,
        params: vec![],
        breakpoints: vec![],
        routine_breakpoints: vec![],
        watches: vec![],
        options: DebugOptions::default(),
        muted: false,
    }
}

pub async fn begin(dialect: ScriptDialect, req: DebugStartRequest) -> Rig {
    begin_with(
        dialect,
        req,
        FakeLease::new(true),
        DebugSettings::default(),
        None,
    )
    .await
}

pub async fn begin_with(
    dialect: ScriptDialect,
    req: DebugStartRequest,
    lease: Arc<FakeLease>,
    settings: DebugSettings,
    routine: Option<Box<dyn RoutineDebugger>>,
) -> Rig {
    let gate = Arc::new(FakeGate::default());
    let sink = Arc::new(RecSink::default());
    let h = start(
        profile(dialect),
        lease.clone(),
        gate.clone(),
        sink.clone(),
        Arc::new(NoProxy),
        settings,
        req,
        routine,
    )
    .await
    .expect("start");
    Rig {
        h,
        lease,
        gate,
        sink,
    }
}

impl Rig {
    pub async fn paused(&self) -> DebugState {
        self.h
            .wait_until(|s| {
                matches!(
                    s.status,
                    DebugStatus::Paused { .. } | DebugStatus::Terminated { .. }
                )
            })
            .await
    }

    pub async fn running(&self) -> DebugState {
        self.h
            .wait_until(|s| matches!(s.status, DebugStatus::Running { .. }))
            .await
    }

    pub async fn step(&self) -> DebugState {
        self.h
            .act(dbstudio_core::debug::DebugAction::StepOver, None)
            .await
            .expect("step");
        self.paused().await
    }

    pub async fn resume(&self) -> DebugState {
        self.h
            .act(dbstudio_core::debug::DebugAction::Resume, None)
            .await
            .expect("resume");
        self.paused().await
    }
}

pub fn reason(s: &DebugState) -> Option<PauseReason> {
    match &s.status {
        DebugStatus::Paused { reason } => Some(reason.clone()),
        _ => None,
    }
}

pub fn terminated(s: &DebugState) -> Option<TerminateReason> {
    match &s.status {
        DebugStatus::Terminated { reason, .. } => Some(*reason),
        _ => None,
    }
}

pub fn step_of(s: &DebugState) -> u32 {
    s.position.as_ref().map(|p| p.step).unwrap_or(u32::MAX)
}

pub fn grant() -> ConfirmationGrant {
    ConfirmationGrant {
        token: TOKEN.into(),
        typed: "app".into(),
    }
}
