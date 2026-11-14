//! The session actor: one tokio task per debug session owning the state machine, the TTL and
//! warning timers (package D).
//!
//! `lib.rs` is frozen at Step 0, so the engine entry point lives here as [`start`] and the client
//! as [`ActorHandle`]; `DebugEngine::start` / `DebugHandle` forward to them.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dbstudio_core::debug::{
    Breakpoint, BreakpointStatus, DebugAction, DebugEndAction, DebugEndReport, DebugFrame,
    DebugOutput, DebugOutputEvent, DebugPosition, DebugSessionCaps, DebugStartRequest, DebugState,
    DebugStatus, DebugTxInfo, DebugTxMode, DebugVariable, EvalContext, EvalResult, EvalValue,
    FrameKind, InlineValue, MessageSource, PauseReason, RoutineBreakpoint, RoutineDebugSupport,
    RoutineRef, RoutineSource, SourceRef, TerminateReason, VariableKind, WatchResult, WatchSpec,
};
use dbstudio_core::{
    ColumnMeta, ConfirmationGrant, DbKind, EndOutcome, Error, ExecSummary, Lease, ParamInput,
    ParamValue, QueryRequest, Result, ResultSink, ScriptDialect, StatementEnd, TxOptions, TxStatus,
    Value,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

use crate::dialect::kit_for;
use crate::eval::{Evaluator, display_value, error_json, value_plain};
use crate::inline;
use crate::program::{DebugProgram, Lexed, Step, StepKind};
use crate::tsql_shim::{Composed, TsqlShim};
use crate::{
    DebugSettings, DebugSink, DialectKit, DialectProfile, GateRequest, ProxyOpener, RoutineCommand,
    RoutineDebugger, RoutineStop, ScriptVar, StepGate, StepRecord,
};

type Reply<T> = oneshot::Sender<Result<T>>;

const TX_ENDED: &str = "the debug transaction was ended by the statement";

enum Cmd {
    Act(DebugAction, Option<ConfirmationGrant>, Reply<DebugState>),
    Stop(Option<DebugEndAction>, Reply<DebugEndReport>),
    Evaluate {
        expr: String,
        frame: Option<u32>,
        echo: bool,
        reply: Reply<EvalResult>,
    },
    SetBreakpoints(SourceRef, Vec<Breakpoint>, Reply<Vec<BreakpointStatus>>),
    SetMuted(bool, Reply<DebugState>),
    SetVariable(String, String, Reply<DebugVariable>),
    Children(String, Reply<Vec<DebugVariable>>),
    SetWatches(Vec<WatchSpec>, Reply<Vec<WatchResult>>),
    SelectFrame(u32, Reply<DebugState>),
    RoutineSource(RoutineRef, Reply<RoutineSource>),
}

/// State shared between the actor task and its clients.
struct Shared {
    pause: AtomicBool,
    stop: AtomicBool,
    query: Mutex<Option<Uuid>>,
    state: Mutex<DebugState>,
    report: Mutex<Option<DebugEndReport>>,
    seq: watch::Sender<u64>,
}

/// Client of one session actor (cheap to clone).
#[derive(Clone)]
pub struct ActorHandle {
    debug_id: Uuid,
    lease_id: Uuid,
    tx: mpsc::Sender<Cmd>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for ActorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorHandle")
            .field("debug_id", &self.debug_id)
            .finish()
    }
}

fn ended() -> Error {
    Error::NotFound("the debug session has ended".into())
}

impl ActorHandle {
    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Cmd) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(make(tx)).await.map_err(|_| ended())?;
        rx.await.map_err(|_| ended())?
    }

    /// Id of the session.
    pub fn debug_id(&self) -> Uuid {
        self.debug_id
    }

    /// Lease the session holds.
    pub fn lease_id(&self) -> Uuid {
        self.lease_id
    }

    /// Latest state snapshot (also answers while a statement runs).
    pub async fn snapshot(&self) -> Result<DebugState> {
        Ok(self.shared.state.lock().unwrap().clone())
    }

    /// Wait until `pred` holds for the published state and return it.
    pub async fn wait_until(&self, pred: impl Fn(&DebugState) -> bool) -> DebugState {
        let mut rx = self.shared.seq.subscribe();
        loop {
            let state = self.shared.state.lock().unwrap().clone();
            if pred(&state) {
                return state;
            }
            if rx.changed().await.is_err() {
                return self.shared.state.lock().unwrap().clone();
            }
        }
    }

    /// Resume / step / run to cursor / jump / pause; returns the `Running` state at once.
    pub async fn act(
        &self,
        action: DebugAction,
        confirm: Option<ConfirmationGrant>,
    ) -> Result<DebugState> {
        self.call(|r| Cmd::Act(action, confirm, r)).await
    }

    /// End the session; `None` = the `endAction` chosen at start. Idempotent.
    pub async fn stop(&self, end: Option<DebugEndAction>) -> Result<DebugEndReport> {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Cmd::Stop(end, tx)).await.is_err() {
            return self.shared.report.lock().unwrap().ok_or_else(ended);
        }
        match rx.await {
            Ok(r) => r,
            Err(_) => self.shared.report.lock().unwrap().ok_or_else(ended),
        }
    }

    /// Evaluate an expression in the read-only frame of `frame` (default: selected).
    pub async fn evaluate(&self, expr: &str, frame: Option<u32>, echo: bool) -> Result<EvalResult> {
        let expr = expr.to_string();
        self.call(|reply| Cmd::Evaluate {
            expr,
            frame,
            echo,
            reply,
        })
        .await
    }

    /// Replace the breakpoints of one source.
    pub async fn set_breakpoints(
        &self,
        source: SourceRef,
        breakpoints: Vec<Breakpoint>,
    ) -> Result<Vec<BreakpointStatus>> {
        self.call(|r| Cmd::SetBreakpoints(source, breakpoints, r))
            .await
    }

    /// Mute / unmute all breakpoints.
    pub async fn set_muted(&self, muted: bool) -> Result<DebugState> {
        self.call(|r| Cmd::SetMuted(muted, r)).await
    }

    /// Change a variable.
    pub async fn set_variable(&self, reference: &str, value_text: &str) -> Result<DebugVariable> {
        let (a, b) = (reference.to_string(), value_text.to_string());
        self.call(|r| Cmd::SetVariable(a, b, r)).await
    }

    /// Children of a variable (lazy tree).
    pub async fn variable_children(&self, reference: &str) -> Result<Vec<DebugVariable>> {
        let a = reference.to_string();
        self.call(|r| Cmd::Children(a, r)).await
    }

    /// Replace the watches and evaluate them when paused.
    pub async fn set_watches(&self, watches: Vec<WatchSpec>) -> Result<Vec<WatchResult>> {
        self.call(|r| Cmd::SetWatches(watches, r)).await
    }

    /// Select a call-stack frame.
    pub async fn select_frame(&self, frame: u32) -> Result<DebugState> {
        self.call(|r| Cmd::SelectFrame(frame, r)).await
    }

    /// Source of a routine through the session's proxy.
    pub async fn routine_source(&self, routine: &RoutineRef) -> Result<RoutineSource> {
        let routine = routine.clone();
        self.call(|r| Cmd::RoutineSource(routine, r)).await
    }
}

// ----------------------------------------------------------------------------------------------
// step execution helpers

/// One result set of a step.
#[derive(Debug, Default)]
struct Group {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
    messages: Vec<String>,
    end: Option<StatementEnd>,
}

/// Buffers what a step returns (rows are capped by the request's `max_rows`).
#[derive(Debug, Default)]
struct StepSink {
    groups: Vec<Group>,
    cur: Group,
    cap: usize,
}

impl StepSink {
    fn new(cap: usize) -> Self {
        StepSink {
            cap,
            ..Default::default()
        }
    }

    fn finish(mut self) -> Vec<Group> {
        if self.cur.end.is_some() || !self.cur.messages.is_empty() || !self.cur.columns.is_empty() {
            self.groups.push(std::mem::take(&mut self.cur));
        }
        self.groups
    }
}

impl ResultSink for StepSink {
    fn on_columns(&mut self, columns: Vec<ColumnMeta>) -> Result<()> {
        self.cur.columns = columns;
        self.cur.rows.clear();
        Ok(())
    }

    fn on_rows(&mut self, mut rows: Vec<Vec<Value>>) -> Result<()> {
        let room = self.cap.saturating_sub(self.cur.rows.len());
        rows.truncate(room);
        self.cur.rows.append(&mut rows);
        Ok(())
    }

    fn on_message(&mut self, message: String) -> Result<()> {
        self.cur.messages.push(message);
        Ok(())
    }

    fn on_statement_end(&mut self, end: StatementEnd) -> Result<()> {
        self.cur.end = Some(end);
        self.groups.push(std::mem::take(&mut self.cur));
        Ok(())
    }
}

#[derive(Debug)]
struct Prepared {
    idx: usize,
    gated_text: String,
    composed: Option<Composed>,
    req: QueryRequest,
    started: Instant,
}

enum StepRes {
    Done,
    Pause(PauseReason),
    Stopped,
    LeaseLost,
    Terminate(Error),
}

enum RunEnd {
    Paused(PauseReason),
    Finished,
    Stopped,
    LeaseLost,
    Terminate(Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunMode {
    Resume,
    Step,
    RunTo { target: usize, force: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoutineAct {
    Into,
    Over,
    Out,
    Continue,
}

enum RunRequest {
    Script {
        mode: RunMode,
        first: bool,
        into: Option<Vec<RoutineRef>>,
    },
    Routine(RoutineAct),
}

struct BpEntry {
    bp: Breakpoint,
    step: Option<usize>,
    hits: u32,
    status: BreakpointStatus,
}

/// A statement running on the target lease while the routine debugger holds it.
struct RoutineCtx {
    handle: JoinHandle<(Result<ExecSummary>, StepSink)>,
    prepared: Prepared,
    at: Option<(RoutineRef, u32, u32)>,
    frames: Vec<DebugFrame>,
}

struct Core {
    debug_id: Uuid,
    profile: DialectProfile,
    dialect: ScriptDialect,
    kit: Box<dyn DialectKit>,
    lease: Arc<dyn Lease>,
    gate: Arc<dyn StepGate>,
    sink: Arc<dyn DebugSink>,
    proxy: Arc<dyn ProxyOpener>,
    settings: DebugSettings,
    request: DebugStartRequest,
    params: Vec<ParamValue>,
    program: DebugProgram,
    shim: Option<TsqlShim>,
    shared: Arc<Shared>,
    seq: u64,
    state: DebugState,
    cursor: usize,
    error_pending: bool,
    pending_confirm: Option<ConfirmationGrant>,
    bps: Vec<BpEntry>,
    routine_bps: Vec<RoutineBreakpoint>,
    muted: bool,
    watches: Vec<WatchSpec>,
    owned: bool,
    tx_since: Option<Instant>,
    warned: bool,
    last_cmd: Instant,
    steps_run: u32,
    committed: bool,
    rolled_back: bool,
    multi_open: bool,
    queued: u32,
    last_result: Option<String>,
    inline: Vec<InlineValue>,
    children: HashMap<String, Vec<DebugVariable>>,
    seen: HashMap<String, (String, u64)>,
    pause_no: u64,
    routine: Option<Box<dyn RoutineDebugger>>,
    routine_ok: bool,
    routine_ctx: Option<RoutineCtx>,
    eval_proxy: Option<Arc<dyn Lease>>,
    old_idle_timeout: Option<String>,
    terminated: bool,
}

fn reply_with<T>(r: Reply<T>, v: Result<T>) {
    let _ = r.send(v);
}

fn text_to_param(text: &str) -> ParamInput {
    let t = text.trim();
    if t.eq_ignore_ascii_case("null") {
        ParamInput::Null
    } else if t.eq_ignore_ascii_case("true") {
        ParamInput::Bool(true)
    } else if t.eq_ignore_ascii_case("false") {
        ParamInput::Bool(false)
    } else if !t.is_empty()
        && t.parse::<f64>().is_ok()
        && t.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
    {
        ParamInput::Number(t.to_string())
    } else if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        ParamInput::Text(t[1..t.len() - 1].replace("''", "'"))
    } else {
        ParamInput::Text(text.to_string())
    }
}

fn param_value(p: &ParamInput) -> Value {
    match p {
        ParamInput::Null => Value::Null,
        ParamInput::Text(s) | ParamInput::Raw(s) => Value::Text(s.clone()),
        ParamInput::Number(n) => n
            .parse::<i64>()
            .map(Value::Int)
            .unwrap_or_else(|_| Value::Decimal(n.clone())),
        ParamInput::Bool(b) => Value::Bool(*b),
    }
}

fn mk_var(
    reference: String,
    name: &str,
    kind: VariableKind,
    value: Value,
    editable: bool,
) -> DebugVariable {
    DebugVariable {
        reference,
        name: name.to_string(),
        kind,
        display: display_value(&value),
        type_name: Some(format!("{:?}", value.kind()).to_lowercase()),
        value: Some(value),
        editable,
        has_children: false,
        changed: false,
    }
}

fn group_var(name: &str) -> DebugVariable {
    DebugVariable {
        reference: format!("g:{name}"),
        name: name.to_string(),
        kind: VariableKind::Group,
        value: None,
        display: String::new(),
        type_name: None,
        editable: false,
        has_children: true,
        changed: false,
    }
}

impl Core {
    // -------------------------------------------------------------------------------- output

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn emit(&mut self, output: DebugOutput) {
        let seq = self.next_seq();
        self.sink.output(DebugOutputEvent {
            debug_id: self.debug_id,
            seq,
            output,
        });
        let _ = self.shared.seq.send(seq);
    }

    fn info(&mut self, text: impl Into<String>) {
        self.emit(DebugOutput::Message {
            text: text.into(),
            source: MessageSource::Info,
        });
    }

    fn message_source(&self) -> MessageSource {
        match self.dialect {
            ScriptDialect::MsSql => MessageSource::Print,
            ScriptDialect::Oracle => MessageSource::DbmsOutput,
            _ => MessageSource::Notice,
        }
    }

    // -------------------------------------------------------------------------------- state

    fn tx_info(&self) -> DebugTxInfo {
        let now = Instant::now();
        let paused = matches!(self.state.status, DebugStatus::Paused { .. });
        DebugTxInfo {
            status: self.lease.state().transaction,
            owned: self.owned,
            open_for_ms: self
                .tx_since
                .map(|t| now.duration_since(t).as_millis() as u64),
            warn: self.warned,
            ttl_left_secs: paused.then(|| {
                let ttl = Duration::from_secs(u64::from(self.settings.paused_ttl_secs));
                (self.last_cmd + ttl)
                    .saturating_duration_since(now)
                    .as_secs()
            }),
        }
    }

    fn caps(&self) -> DebugSessionCaps {
        DebugSessionCaps {
            step_into: self.routine_ok,
            step_out: self.routine_ctx.is_some(),
            pause_running: true,
            set_variable: true,
            routine: self.routine_ok,
            jump_to: true,
        }
    }

    fn script_frame(&self, idx: usize) -> DebugFrame {
        let step = &self.program.steps[idx];
        let name = match &step.kind {
            StepKind::Block { reason } => format!("script:{} — {reason}", step.line_start),
            _ => format!(
                "script:{} — {} ({}/{})",
                step.line_start,
                step.info.verb,
                idx + 1,
                self.program.steps.len()
            ),
        };
        DebugFrame {
            id: 0,
            kind: if matches!(step.kind, StepKind::Block { .. }) {
                FrameKind::Block
            } else {
                FrameKind::Script
            },
            name,
            source: SourceRef::Script,
            line: step.line_start,
            language: None,
            args: None,
        }
    }

    fn current_idx(&self) -> Option<usize> {
        if self.program.steps.is_empty() {
            None
        } else {
            Some(self.cursor.min(self.program.steps.len() - 1))
        }
    }

    /// Publish the state (a new `seq`).
    fn publish(&mut self) {
        let seq = self.next_seq();
        let mut state = self.state.clone();
        state.seq = seq;
        state.transaction = self.tx_info();
        state.caps = self.caps();
        state.muted = self.muted;
        state.breakpoints = self.bps.iter().map(|b| b.status.clone()).collect();
        state.inline_values = if self.settings.inline_values {
            self.inline.clone()
        } else {
            vec![]
        };
        self.state = state.clone();
        *self.shared.state.lock().unwrap() = state;
        self.sink.state(&self.state);
        let _ = self.shared.seq.send(seq);
    }

    fn set_status(&mut self, status: DebugStatus) {
        self.state.status = status;
    }

    fn groups(&self) -> Vec<DebugVariable> {
        let mut out = Vec::new();
        if !self.params.is_empty() {
            out.push(group_var("Parameters"));
        }
        if self.shim.is_some() || !self.kit.script_variables(&self.program).is_empty() {
            out.push(group_var("Script variables"));
        }
        out.push(group_var("Session"));
        let probes = self.kit.probes();
        for g in ["Settings", "Temporary tables"] {
            if probes.iter().any(|p| p.group == g) {
                out.push(group_var(g));
            }
        }
        if self.profile.caps.manual_transactions {
            out.push(group_var("Transaction"));
        }
        out.push(group_var("Last result"));
        if self.routine_ctx.is_some() {
            out.push(group_var("Routine"));
        }
        out
    }

    fn eval_skip(&self) -> Option<String> {
        if self.multi_open {
            return Some("MULTI is open: the command would be queued".into());
        }
        if let Some(next) = self.program.steps.get(self.cursor) {
            if self.kit.preserves_counters(next) {
                return Some("keeping ROW_COUNT() for the next statement".into());
            }
        }
        None
    }

    fn evaluator<'a>(&'a self, skip: Option<&'a str>) -> Evaluator<'a> {
        Evaluator {
            lease: &*self.lease,
            gate: &*self.gate,
            kit: &*self.kit,
            dialect: self.dialect,
            timeout_ms: self.settings.watch_timeout_ms,
            skip,
        }
    }

    async fn eval_watches(&mut self) -> Vec<WatchResult> {
        let skip = self.eval_skip();
        let mut out = Vec::new();
        for w in self.watches.clone() {
            let result = self.eval_expression(&w.expr, skip.as_deref()).await;
            out.push(WatchResult {
                id: w.id.clone(),
                expr: w.expr.clone(),
                result,
            });
        }
        out
    }

    /// Variable of the selected routine frame named like the expression.
    async fn routine_variable(&mut self, expr: &str) -> Option<EvalResult> {
        let started = Instant::now();
        let r = self.routine.as_mut()?;
        let vars = r.variables().await.ok()?;
        let v = vars
            .iter()
            .find(|v| v.name.eq_ignore_ascii_case(expr.trim()))?;
        Some(EvalResult {
            value: EvalValue::Scalar {
                value: v.value.clone().unwrap_or(Value::Null),
                type_name: v.type_name.clone().unwrap_or_default(),
            },
            context: EvalContext::RoutineVariable,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    async fn eval_expression(&mut self, expr: &str, skip: Option<&str>) -> EvalResult {
        if self.routine_ctx.is_some() {
            if let Some(r) = self.routine_variable(expr).await {
                return r;
            }
            return self.eval_on_proxy(expr).await;
        }
        self.evaluator(skip).eval_expr(expr).await
    }

    /// Expression inside a routine: the target is blocked, so it runs on a proxy lease with the
    /// routine variables replaced by literals.
    async fn eval_on_proxy(&mut self, expr: &str) -> EvalResult {
        let started = Instant::now();
        let failed = |e: &Error| EvalResult {
            value: EvalValue::Error {
                error: error_json(e),
            },
            context: EvalContext::Proxy,
            elapsed_ms: started.elapsed().as_millis() as u64,
        };
        if self.eval_proxy.is_none() {
            let db = self.lease.state().database;
            match self.proxy.open_proxy(db).await {
                Ok(l) => self.eval_proxy = Some(l),
                Err(e) => return failed(&e),
            }
        }
        let vars = match self.routine.as_mut() {
            Some(r) => r.variables().await.unwrap_or_default(),
            None => vec![],
        };
        let lexed = Lexed::new(self.dialect, expr);
        let mut text = String::new();
        let mut pos = 0usize;
        for (k, s, e) in &lexed.toks {
            if *k != dbstudio_sql::lexer::TokenKind::Word {
                continue;
            }
            let w = lexed.slice(*s, *e);
            if let Some(v) = vars.iter().find(|v| v.name.eq_ignore_ascii_case(&w)) {
                text.push_str(&lexed.slice(pos, *s));
                let lit = match v.value.as_ref() {
                    None | Some(Value::Null) => "NULL".to_string(),
                    Some(
                        n @ (Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_)),
                    ) => value_plain(n),
                    Some(Value::Bool(b)) => if *b { "true" } else { "false" }.into(),
                    Some(other) => format!("'{}'", value_plain(other).replace('\'', "''")),
                };
                text.push_str(&lit);
                pos = *e;
            }
        }
        text.push_str(&lexed.slice(pos, lexed.chars.len()));
        let Some(proxy) = self.eval_proxy.clone() else {
            return failed(&Error::Other("no proxy".into()));
        };
        let ev = Evaluator {
            lease: &*proxy,
            gate: &*self.gate,
            kit: &*self.kit,
            dialect: self.dialect,
            timeout_ms: self.settings.watch_timeout_ms,
            skip: None,
        };
        let mut r = ev.eval_expr(&text).await;
        r.context = EvalContext::Proxy;
        r
    }

    /// Frames, variables, watches, step-into targets and the Paused state.
    async fn enter_pause(&mut self, reason: PauseReason) {
        self.last_cmd = Instant::now();
        self.pause_no += 1;
        self.children.clear();
        self.refresh_tx();
        let at_end = matches!(reason, PauseReason::AwaitingEnd);
        if !at_end {
            while self.cursor < self.program.steps.len()
                && !self.program.steps[self.cursor].is_stop()
            {
                if self.program.steps[self.cursor].kind == StepKind::Meta {
                    self.run_meta(self.cursor).await;
                }
                self.cursor += 1;
            }
        }
        let idx = self.current_idx().map(|i| {
            if at_end {
                self.program.steps.len() - 1
            } else {
                i
            }
        });
        let total = self.program.steps.len() as u32;
        let mut frames: Vec<DebugFrame> = vec![];
        let mut position = None;
        if let Some(ctx) = &self.routine_ctx {
            frames = ctx.frames.clone();
            position = ctx.at.as_ref().map(|(r, line, _)| DebugPosition {
                source: SourceRef::Routine { routine: r.clone() },
                line: *line,
                range: None,
                step: idx.unwrap_or(0) as u32,
                step_count: total,
            });
        }
        let script_pos = idx.map(|i| {
            let s = &self.program.steps[i];
            DebugPosition {
                source: SourceRef::Script,
                line: s.line_start,
                range: Some(s.range),
                step: i as u32,
                step_count: total,
            }
        });
        if let Some(i) = idx {
            frames.push(self.script_frame(i));
        }
        for (n, f) in frames.iter_mut().enumerate() {
            f.id = n as u32;
        }
        self.state.frames = frames;
        self.state.selected_frame = 0;
        self.state.position = position.or(script_pos);
        self.state.variables = self.groups();
        self.set_status(DebugStatus::Paused { reason });
        // groups that load at once
        for p in self.kit.probes().into_iter().filter(|p| p.always) {
            let _ = self.load_children(&format!("g:{}", p.group)).await;
        }
        self.state.watches = self.eval_watches().await;
        self.state.step_into_targets = vec![];
        if self.routine_ok && self.routine_ctx.is_none() && !at_end {
            if let (Some(i), Some(r)) = (idx, self.routine.as_mut()) {
                let step = self.program.steps[i].clone();
                if step.is_stop() && self.cursor < self.program.steps.len() {
                    if let Ok(t) = r.step_into_targets(&*self.lease, &step, false).await {
                        self.state.step_into_targets = t;
                    }
                }
            }
        }
        self.publish();
    }

    fn refresh_tx(&mut self) {
        match self.lease.state().transaction {
            TxStatus::Idle => {
                self.tx_since = None;
                if self.owned {
                    self.owned = false;
                    self.info(TX_ENDED);
                }
            }
            _ => {
                if self.tx_since.is_none() {
                    self.tx_since = Some(Instant::now());
                }
            }
        }
    }

    // ------------------------------------------------------------------------------ variables

    async fn load_children(&mut self, reference: &str) -> Result<Vec<DebugVariable>> {
        if let Some(v) = self.children.get(reference) {
            return Ok(v.clone());
        }
        let Some(group) = reference.strip_prefix("g:") else {
            return Err(Error::NotFound(format!("variable {reference}")));
        };
        let mut vars: Vec<DebugVariable> = match group {
            "Parameters" => self
                .params
                .iter()
                .map(|p| {
                    mk_var(
                        format!("p:{}", p.name),
                        &p.name,
                        VariableKind::Parameter,
                        param_value(&p.value),
                        true,
                    )
                })
                .collect(),
            "Script variables" => self.script_var_children().await,
            "Session" => self.session_children().await,
            "Transaction" => self.transaction_children(),
            "Last result" => vec![mk_var(
                "lr:summary".into(),
                "result",
                VariableKind::Result,
                Value::Text(
                    self.last_result
                        .clone()
                        .unwrap_or_else(|| "no statement ran yet".into()),
                ),
                false,
            )],
            "Routine" => match self.routine.as_mut() {
                Some(r) => r.variables().await?,
                None => vec![],
            },
            other => self.probe_children(other).await,
        };
        // highlight what changed since the previous pause
        for v in vars.iter_mut() {
            let key = format!("{group}/{}", v.name);
            if let Some((old, no)) = self.seen.get(&key) {
                if *no < self.pause_no && *old != v.display {
                    v.changed = true;
                }
            }
            if self
                .seen
                .get(&key)
                .is_none_or(|(_, no)| *no < self.pause_no)
            {
                self.seen.insert(key, (v.display.clone(), self.pause_no));
            }
        }
        self.children.insert(reference.to_string(), vars.clone());
        Ok(vars)
    }

    async fn script_var_children(&mut self) -> Vec<DebugVariable> {
        if let Some(shim) = &self.shim {
            let mut out: Vec<DebugVariable> = shim
                .vars()
                .iter()
                .map(|v| {
                    let mut var = mk_var(
                        format!("sv:{}", v.name),
                        &v.name,
                        VariableKind::ScriptVariable,
                        v.value.clone().unwrap_or(Value::Null),
                        true,
                    );
                    var.type_name = Some(v.ty.clone());
                    var
                })
                .collect();
            for sv in self.kit.script_variables(&self.program) {
                if !out.iter().any(|v| v.name.eq_ignore_ascii_case(&sv.name)) {
                    let mut var = mk_var(
                        format!("sv:{}", sv.name),
                        &sv.name,
                        VariableKind::ScriptVariable,
                        Value::Text("(not declared yet)".into()),
                        false,
                    );
                    var.type_name = sv.declared_type;
                    out.push(var);
                }
            }
            return out;
        }
        let names: Vec<String> = self
            .kit
            .script_variables(&self.program)
            .into_iter()
            .map(|v| v.name)
            .collect();
        if names.is_empty() {
            return vec![];
        }
        let skip = self.eval_skip();
        let sql = crate::dialect::mysql::vars_query(&names);
        let rows = self.evaluator(skip.as_deref()).rows(&sql).await;
        match rows {
            Ok(rs) => names
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    let v = rs
                        .rows
                        .first()
                        .and_then(|r| r.get(i))
                        .cloned()
                        .unwrap_or(Value::Null);
                    mk_var(format!("sv:{n}"), n, VariableKind::ScriptVariable, v, true)
                })
                .collect(),
            Err(reason) => vec![mk_var(
                "sv:-".into(),
                "variables",
                VariableKind::ScriptVariable,
                Value::Text(format!("({reason})")),
                false,
            )],
        }
    }

    async fn session_children(&mut self) -> Vec<DebugVariable> {
        let st = self.lease.state();
        let sv = |n: &str, v: Value| mk_var(format!("s:{n}"), n, VariableKind::Session, v, false);
        let mut out = vec![];
        if let Some(db) = st.database {
            out.push(sv("database", Value::Text(db)));
        }
        if let Some(s) = st.schema {
            out.push(sv("schema", Value::Text(s)));
        }
        out.push(sv("autocommit", Value::Bool(st.auto_commit)));
        if self.multi_open {
            out.push(sv("MULTI", Value::Text(format!("{} queued", self.queued))));
        }
        let skip = self.eval_skip();
        let probes: Vec<_> = self
            .kit
            .probes()
            .into_iter()
            .filter(|p| p.group == "Session")
            .collect();
        for p in probes {
            if let Ok(rs) = self.evaluator(skip.as_deref()).rows(&p.sql).await {
                let Some(row) = rs.rows.first() else { continue };
                if rs.columns.len() == 1 && self.dialect == ScriptDialect::RedisCli {
                    // CLIENT INFO: one text of `key=value` pairs
                    if let Some(Value::Text(t)) = row.first() {
                        for kv in t.split_whitespace() {
                            if let Some((k, v)) = kv.split_once('=') {
                                out.push(sv(k, Value::Text(v.into())));
                            }
                        }
                    }
                    continue;
                }
                for (c, v) in rs.columns.iter().zip(row.iter()) {
                    out.push(sv(&c.name, v.clone()));
                }
            }
        }
        out
    }

    fn transaction_children(&self) -> Vec<DebugVariable> {
        let info = self.tx_info();
        let t = |r: &str, n: &str, v: Value| {
            mk_var(format!("t:{r}"), n, VariableKind::Transaction, v, false)
        };
        let mut out = vec![
            t(
                "status",
                "status",
                Value::Text(format!("{:?}", info.status).to_lowercase()),
            ),
            t("owned", "owned by the debugger", Value::Bool(info.owned)),
        ];
        if let Some(ms) = info.open_for_ms {
            out.push(t("open", "open for (ms)", Value::Int(ms as i64)));
        }
        out
    }

    async fn probe_children(&mut self, group: &str) -> Vec<DebugVariable> {
        let skip = self.eval_skip();
        let editable = group == "Settings"
            && matches!(
                self.dialect,
                ScriptDialect::Postgres | ScriptDialect::ClickHouse
            );
        let (kind, prefix) = if group == "Settings" {
            (VariableKind::Setting, "st")
        } else {
            (VariableKind::TempTable, "tt")
        };
        let probes: Vec<_> = self
            .kit
            .probes()
            .into_iter()
            .filter(|p| p.group == group)
            .collect();
        let mut out = vec![];
        for p in probes {
            match self.evaluator(skip.as_deref()).rows(&p.sql).await {
                Ok(rs) => {
                    for row in rs.rows {
                        let name = row.first().map(value_plain).unwrap_or_default();
                        let value = row.get(1).cloned().unwrap_or(Value::Null);
                        let mut v =
                            mk_var(format!("{prefix}:{name}"), &name, kind, value, editable);
                        if row.len() < 2 {
                            v.display = "temporary table".into();
                        }
                        out.push(v);
                    }
                }
                Err(reason) => out.push(mk_var(
                    format!("{group}:-"),
                    group,
                    kind,
                    Value::Text(format!("({reason})")),
                    false,
                )),
            }
        }
        out
    }

    async fn set_variable(&mut self, reference: &str, value_text: &str) -> Result<DebugVariable> {
        if let Some(name) = reference.strip_prefix("p:") {
            let input = text_to_param(value_text);
            match self.params.iter_mut().find(|p| p.name == name) {
                Some(p) => p.value = input.clone(),
                None => return Err(Error::NotFound(format!("parameter {name}"))),
            }
            self.children.remove("g:Parameters");
            return Ok(mk_var(
                reference.into(),
                name,
                VariableKind::Parameter,
                param_value(&input),
                true,
            ));
        }
        if let Some(name) = reference.strip_prefix("sv:") {
            if let Some(shim) = self.shim.as_mut() {
                if !shim.set_value(name, value_text) {
                    return Err(Error::NotFound(format!(
                        "variable {name} is not declared yet"
                    )));
                }
                self.children.remove("g:Script variables");
                let value = shim
                    .vars()
                    .iter()
                    .find(|v| v.name.eq_ignore_ascii_case(name))
                    .and_then(|v| v.value.clone())
                    .unwrap_or(Value::Null);
                return Ok(mk_var(
                    reference.into(),
                    name,
                    VariableKind::ScriptVariable,
                    value,
                    true,
                ));
            }
            let var = ScriptVar {
                name: name.into(),
                declared_type: None,
                line: 0,
            };
            let sql = self.kit.set_variable_sql(&var, value_text)?;
            self.run_set(&sql).await?;
            self.children.remove("g:Script variables");
            return Ok(mk_var(
                reference.into(),
                name,
                VariableKind::ScriptVariable,
                Value::Text(value_text.to_string()),
                true,
            ));
        }
        if let Some(name) = reference.strip_prefix("st:") {
            let var = ScriptVar {
                name: name.into(),
                declared_type: None,
                line: 0,
            };
            let sql = self.kit.set_variable_sql(&var, value_text)?;
            self.run_set(&sql).await?;
            self.children.remove("g:Settings");
            return Ok(mk_var(
                reference.into(),
                name,
                VariableKind::Setting,
                Value::Text(value_text.to_string()),
                true,
            ));
        }
        if self.routine_ctx.is_some() {
            if let Some(r) = self.routine.as_mut() {
                self.children.remove("g:Routine");
                return r.set_variable(reference, value_text).await;
            }
        }
        Err(Error::Invalid(format!(
            "variable {reference} cannot be changed"
        )))
    }

    /// Run a variable assignment through the guard like a script step.
    async fn run_set(&mut self, sql: &str) -> Result<()> {
        let st = self.lease.state();
        let gated = self.gate.check(GateRequest {
            dialect: self.dialect,
            text: sql,
            params: &[],
            confirm: None,
            database: st.database.as_deref(),
            schema: st.schema.as_deref(),
        })?;
        let mut sink = StepSink::new(1);
        self.lease
            .execute(
                QueryRequest::new(self.kit.wrap_single(&gated.text)),
                &mut sink,
            )
            .await?;
        Ok(())
    }

    // ------------------------------------------------------------------------------ breakpoints

    fn apply_breakpoints(&mut self, bps: Vec<Breakpoint>) -> Vec<BreakpointStatus> {
        let old: HashMap<String, u32> =
            self.bps.iter().map(|b| (b.bp.id.clone(), b.hits)).collect();
        self.bps = bps
            .into_iter()
            .map(|bp| {
                let snap = self.program.snap(bp.line);
                let status = BreakpointStatus {
                    id: bp.id.clone(),
                    verified: snap.step.is_some(),
                    line: snap.line,
                    message: snap.message,
                };
                BpEntry {
                    hits: old.get(&bp.id).copied().unwrap_or(0),
                    step: snap.step,
                    bp,
                    status,
                }
            })
            .collect();
        self.bps.iter().map(|b| b.status.clone()).collect()
    }

    fn bp_hits_step(&self, e: &BpEntry, idx: usize) -> bool {
        e.step
            .is_some_and(|s| self.program.steps[s].range == self.program.steps[idx].range)
    }

    async fn check_breakpoints(&mut self, idx: usize) -> Option<RunEnd> {
        if self.muted {
            return None;
        }
        let candidates: Vec<usize> = (0..self.bps.len())
            .filter(|i| self.bps[*i].bp.enabled && self.bp_hits_step(&self.bps[*i], idx))
            .collect();
        if candidates.is_empty() {
            return None;
        }
        if self.kit.preserves_counters(&self.program.steps[idx])
            && candidates
                .iter()
                .any(|i| self.bps[*i].bp.condition.is_some())
        {
            self.info("the breakpoint condition runs before a statement that reads ROW_COUNT(): the counters are reset");
        }
        let mut stop_ids = vec![];
        for i in candidates {
            let bp = self.bps[i].bp.clone();
            let mut hit = true;
            if let Some(c) = bp
                .condition
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
            {
                let r = self.evaluator(None).condition(c).await;
                match r {
                    Ok(b) => hit = b,
                    Err(e) => {
                        // a failing condition stops, like JetBrains
                        self.info(format!("breakpoint condition failed: {}", e.message()));
                        stop_ids.push(bp.id.clone());
                        continue;
                    }
                }
            }
            if !hit {
                continue;
            }
            self.bps[i].hits += 1;
            if let Some(n) = bp.hit_count {
                if self.bps[i].hits < n {
                    continue;
                }
            }
            if let Some(t) = bp.log_message.as_deref().filter(|t| !t.is_empty()) {
                let text = self.evaluator(None).render(t).await;
                self.emit(DebugOutput::Message {
                    text,
                    source: MessageSource::Log,
                });
            }
            if bp.suspend {
                stop_ids.push(bp.id.clone());
            }
        }
        (!stop_ids.is_empty()).then_some(RunEnd::Paused(PauseReason::Breakpoint { ids: stop_ids }))
    }

    // --------------------------------------------------------------------------------- stepping

    fn timeout(&self) -> Option<u64> {
        let t = self
            .request
            .options
            .statement_timeout_ms
            .unwrap_or(self.settings.query_timeout_ms);
        (t > 0).then_some(t)
    }

    fn max_rows(&self) -> u32 {
        self.request
            .options
            .max_rows_per_step
            .unwrap_or(self.settings.max_rows_per_step)
    }

    async fn run_meta(&mut self, idx: usize) {
        let step = self.program.steps[idx].clone();
        let w = self.program.words(&step, 2);
        if self.dialect == ScriptDialect::Oracle
            && w.first().map(String::as_str) == Some("SET")
            && w.get(1).map(String::as_str) == Some("SERVEROUTPUT")
        {
            let mut sink = StepSink::new(1);
            let _ = self
                .lease
                .execute(QueryRequest::new(step.text.clone()), &mut sink)
                .await;
        }
    }

    /// Gate, compose and announce a step; `Err` is the early outcome (blocked, error).
    async fn prepare(&mut self, idx: usize) -> std::result::Result<Prepared, StepRes> {
        if self.lease.state().broken {
            return Err(StepRes::LeaseLost);
        }
        let step = self.program.steps[idx].clone();
        let st = self.lease.state();
        let confirm = self.pending_confirm.take();
        let gated = self.gate.check(GateRequest {
            dialect: self.dialect,
            text: &step.text,
            params: &self.params,
            confirm: confirm.as_ref(),
            database: st.database.as_deref(),
            schema: st.schema.as_deref(),
        });
        let gated = match gated {
            Ok(g) => g,
            Err(e) => return Err(self.step_failure(idx, e)),
        };
        if self.owned && step.info.implicit_commit {
            self.info("this statement commits the debug transaction implicitly");
        }
        self.emit(DebugOutput::StepStart {
            step: idx as u32,
            line: step.line_start,
            text: step.text.clone(),
        });
        let composed = self.shim.as_mut().map(|s| s.compose(&step, &gated.text));
        let text = match &composed {
            Some(c) => c.text.clone(),
            None => self.kit.wrap_single(&gated.text),
        };
        let mut req = QueryRequest::new(text);
        req.max_rows = Some(u64::from(self.max_rows()));
        req.timeout_ms = self.timeout();
        *self.shared.query.lock().unwrap() = Some(req.query_id);
        Ok(Prepared {
            idx,
            gated_text: gated.text,
            composed,
            req,
            started: Instant::now(),
        })
    }

    /// Classify a failed statement / guard refusal.
    fn step_failure(&mut self, idx: usize, e: Error) -> StepRes {
        match &e {
            Error::ReadOnlyViolation { .. } => {
                self.emit(DebugOutput::Error {
                    step: Some(idx as u32),
                    error: error_json(&e),
                });
                return StepRes::Pause(PauseReason::Blocked {
                    error: error_json(&e),
                });
            }
            Error::ConfirmationRequired(req) => {
                return StepRes::Pause(PauseReason::Confirmation {
                    request: (**req).clone(),
                });
            }
            _ => {}
        }
        if self.shared.stop.load(Ordering::SeqCst) && matches!(e, Error::Cancelled) {
            return StepRes::Stopped;
        }
        if self.lease.state().broken {
            return StepRes::LeaseLost;
        }
        let step = self.program.steps[idx].clone();
        self.emit(DebugOutput::Error {
            step: Some(idx as u32),
            error: error_json(&e),
        });
        inline::merge(&mut self.inline, inline::error_inline(&step, &e.message()));
        self.last_result = Some(format!("error: {}", e.message()));
        if self.request.options.break_on_error {
            self.error_pending = true;
            StepRes::Pause(PauseReason::Error {
                error: error_json(&e),
            })
        } else {
            StepRes::Terminate(e)
        }
    }

    fn record(&self, p: &Prepared, rows: Option<u64>, error: Option<String>) {
        if !self.request.options.record_history {
            return;
        }
        self.gate.record(&StepRecord {
            dialect: self.dialect,
            text: p.gated_text.clone(),
            database: self.lease.state().database,
            elapsed_ms: p.started.elapsed().as_millis() as u64,
            rows,
            error,
        });
    }

    /// Process the answer of a statement: console, inline values, variables, history.
    async fn complete(&mut self, p: Prepared, res: Result<ExecSummary>, sink: StepSink) -> StepRes {
        *self.shared.query.lock().unwrap() = None;
        self.steps_run += 1;
        let idx = p.idx;
        let step = self.program.steps[idx].clone();
        let mut groups = sink.finish();
        let summary = match res {
            Err(e) => {
                self.record(&p, None, Some(e.message()));
                self.refresh_tx();
                return self.step_failure(idx, e);
            }
            Ok(s) => s,
        };
        let _ = summary;
        // the shim read-out is the last result set with an end: swallow it
        if let Some(c) = &p.composed {
            if !c.readout.is_empty() {
                if let Some(pos) = groups.iter().rposition(|g| g.end.is_some()) {
                    let g = groups.remove(pos);
                    if let (Some(row), Some(shim)) = (g.rows.first(), self.shim.as_mut()) {
                        shim.absorb(c, row);
                        self.children.remove("g:Script variables");
                    }
                }
            }
        }
        let source = self.message_source();
        let mut last_end: Option<StatementEnd> = None;
        let mut first_row: Option<(Vec<ColumnMeta>, Vec<Value>)> = None;
        for g in &groups {
            if !g.columns.is_empty() {
                self.emit(DebugOutput::Query {
                    step: idx as u32,
                    event: dbstudio_core::QueryEvent::Columns {
                        columns: g.columns.clone(),
                    },
                });
            }
            if !g.rows.is_empty() {
                self.emit(DebugOutput::Query {
                    step: idx as u32,
                    event: dbstudio_core::QueryEvent::Rows {
                        rows: g.rows.clone(),
                    },
                });
            }
            for m in &g.messages {
                self.emit(DebugOutput::Message {
                    text: m.clone(),
                    source,
                });
            }
            if let Some(end) = &g.end {
                last_end = Some(end.clone());
                first_row = g.rows.first().map(|r| (g.columns.clone(), r.clone()));
            }
        }
        let end = last_end.unwrap_or(StatementEnd {
            outcome: EndOutcome::Done,
            elapsed_ms: p.started.elapsed().as_millis() as u64,
        });
        let truncated_at = matches!(
            end.outcome,
            EndOutcome::Rows {
                truncated: true,
                ..
            }
        )
        .then(|| self.max_rows());
        self.emit(DebugOutput::StepEnd {
            step: idx as u32,
            end: end.clone(),
            truncated_at,
        });
        if let Some(t) = truncated_at {
            self.info(format!("result truncated at {t} rows"));
        }
        let rows = match &end.outcome {
            EndOutcome::Rows { row_count, .. } => Some(*row_count),
            EndOutcome::Affected { rows } => Some(*rows),
            EndOutcome::Done => None,
        };
        self.record(&p, rows, None);
        self.last_result = Some(match &end.outcome {
            EndOutcome::Rows {
                row_count,
                truncated,
            } => format!(
                "{row_count}{} rows in {} ms",
                if *truncated { "+" } else { "" },
                end.elapsed_ms
            ),
            EndOutcome::Affected { rows } => {
                format!("{rows} rows affected in {} ms", end.elapsed_ms)
            }
            EndOutcome::Done => format!("done in {} ms", end.elapsed_ms),
        });
        if let Some(v) = inline::result_inline(
            &step,
            &end,
            first_row
                .as_ref()
                .map(|(c, r)| (c.as_slice(), r.as_slice())),
        ) {
            inline::merge(&mut self.inline, v);
        }
        self.after_step(&step, idx).await;
        self.refresh_tx();
        StepRes::Done
    }

    /// `MULTI` tracking (Redis) and variable inline values (T-SQL shim, MySQL `@x`).
    async fn after_step(&mut self, step: &Step, idx: usize) {
        if self.dialect == ScriptDialect::RedisCli {
            let w = self.program.words(step, 1);
            match w.first().map(String::as_str) {
                Some("MULTI") => {
                    self.multi_open = true;
                    self.queued = 0;
                }
                Some("EXEC" | "DISCARD") => self.multi_open = false,
                _ if self.multi_open => self.queued += 1,
                _ => {}
            }
        }
        let upper = step.text.to_uppercase();
        if let Some(shim) = &self.shim {
            let vals: Vec<(String, String)> = shim
                .vars()
                .iter()
                .filter(|v| upper.contains(&v.name.to_uppercase()))
                .map(|v| {
                    (
                        v.name.clone(),
                        display_value(v.value.as_ref().unwrap_or(&Value::Null)),
                    )
                })
                .collect();
            for (n, d) in vals {
                inline::merge(&mut self.inline, inline::variable_inline(step, &n, &d));
            }
        } else if matches!(self.dialect, ScriptDialect::MySql { .. }) {
            let assigns = upper.starts_with("SET @") || upper.contains(" INTO @");
            let next = self.program.steps.get(idx + 1).unwrap_or(step);
            if assigns && !self.kit.preserves_counters(next) {
                let names: Vec<String> = self
                    .program
                    .at_variables()
                    .into_iter()
                    .map(|v| v.0)
                    .filter(|n| upper.contains(&n.to_uppercase()))
                    .collect();
                if !names.is_empty() {
                    let sql = crate::dialect::mysql::vars_query(&names);
                    if let Ok(rs) = self.evaluator(None).rows(&sql).await {
                        for (i, n) in names.iter().enumerate() {
                            if let Some(v) = rs.rows.first().and_then(|r| r.get(i)) {
                                inline::merge(
                                    &mut self.inline,
                                    inline::variable_inline(step, n, &display_value(v)),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    async fn exec_step(&mut self, idx: usize) -> StepRes {
        let p = match self.prepare(idx).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        let mut sink = StepSink::new(self.max_rows() as usize);
        let res = self.lease.execute(p.req.clone(), &mut sink).await;
        self.complete(p, res, sink).await
    }

    /// Execute steps from the cursor until something pauses or the script ends.
    async fn run_steps(&mut self, mode: RunMode, mut first: bool) -> RunEnd {
        if self.error_pending {
            self.error_pending = false;
            self.cursor += 1;
        }
        loop {
            while self.cursor < self.program.steps.len()
                && !self.program.steps[self.cursor].is_stop()
            {
                if self.program.steps[self.cursor].kind == StepKind::Meta {
                    self.run_meta(self.cursor).await;
                }
                self.cursor += 1;
            }
            let idx = self.cursor;
            if idx >= self.program.steps.len() {
                return RunEnd::Finished;
            }
            if !first {
                if self.shared.stop.load(Ordering::SeqCst) {
                    return RunEnd::Stopped;
                }
                if self.shared.pause.swap(false, Ordering::SeqCst) {
                    return RunEnd::Paused(PauseReason::Pause);
                }
                if let RunMode::RunTo { target, .. } = mode {
                    if self.program.steps[target].range == self.program.steps[idx].range {
                        return RunEnd::Paused(PauseReason::RunToCursor);
                    }
                }
                if !matches!(mode, RunMode::RunTo { force: true, .. }) {
                    if let Some(end) = self.check_breakpoints(idx).await {
                        return end;
                    }
                }
            }
            first = false;
            match self.exec_step(idx).await {
                StepRes::Done => self.cursor = idx + 1,
                StepRes::Pause(r) => return RunEnd::Paused(r),
                StepRes::Stopped => return RunEnd::Stopped,
                StepRes::LeaseLost => return RunEnd::LeaseLost,
                StepRes::Terminate(e) => return RunEnd::Terminate(e),
            }
            if mode == RunMode::Step {
                return match self.program.next_stop(self.cursor) {
                    Some(_) => RunEnd::Paused(PauseReason::Step),
                    None => RunEnd::Finished,
                };
            }
        }
    }

    // -------------------------------------------------------------------------- routine stepping

    async fn run_step_into(&mut self, targets: Vec<RoutineRef>) -> RunEnd {
        let idx = self.cursor;
        let p = match self.prepare(idx).await {
            Ok(p) => p,
            Err(StepRes::Pause(r)) => return RunEnd::Paused(r),
            Err(StepRes::LeaseLost) => return RunEnd::LeaseLost,
            Err(StepRes::Terminate(e)) => return RunEnd::Terminate(e),
            Err(_) => return RunEnd::Stopped,
        };
        let bps = self.routine_bps.clone();
        if let Some(r) = self.routine.as_mut() {
            if let Err(e) = r.arm(&targets, &bps).await {
                let msg = format!("cannot arm the routine debugger: {}", e.message());
                self.info(msg);
            }
        }
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let lease = self.lease.clone();
        let req = p.req.clone();
        let cap = self.max_rows() as usize;
        let handle = tokio::spawn(async move {
            let mut sink = StepSink::new(cap);
            let r = lease.execute(req, &mut sink).await;
            let _ = done_tx.send(());
            (r, sink)
        });
        let mut done: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(async move {
            let _ = done_rx.await;
        });
        self.routine_ctx = Some(RoutineCtx {
            handle,
            prepared: p,
            at: None,
            frames: vec![],
        });
        let stop = match self.routine.as_mut() {
            Some(r) => r.wait(&mut done).await,
            None => Ok(RoutineStop::TargetDone),
        };
        self.handle_routine_stop(stop, RoutineAct::Into, true).await
    }

    async fn handle_routine_stop(
        &mut self,
        stop: Result<RoutineStop>,
        act: RoutineAct,
        entering: bool,
    ) -> RunEnd {
        match stop {
            Ok(RoutineStop::At {
                routine,
                line,
                depth,
            }) => {
                let frames = match self.routine.as_mut() {
                    Some(r) => r.frames().await.unwrap_or_default(),
                    None => vec![],
                };
                if let Some(ctx) = self.routine_ctx.as_mut() {
                    ctx.at = Some((routine, line, depth));
                    ctx.frames = frames;
                }
                RunEnd::Paused(match act {
                    _ if entering => PauseReason::RoutineEntry,
                    RoutineAct::Continue => PauseReason::Breakpoint { ids: vec![] },
                    _ => PauseReason::Step,
                })
            }
            Ok(RoutineStop::TargetDone | RoutineStop::Aborted) | Err(_) => {
                self.finish_routine_step(act).await
            }
        }
    }

    /// The statement on the target finished: complete the script step.
    async fn finish_routine_step(&mut self, act: RoutineAct) -> RunEnd {
        let Some(ctx) = self.routine_ctx.take() else {
            return RunEnd::Finished;
        };
        if let Some(r) = self.routine.as_mut() {
            let _ = r.disarm().await;
        }
        let (res, sink) = match ctx.handle.await {
            Ok(x) => x,
            Err(e) => (
                Err(Error::Other(format!("statement task failed: {e}"))),
                StepSink::default(),
            ),
        };
        let idx = ctx.prepared.idx;
        match self.complete(ctx.prepared, res, sink).await {
            StepRes::Done => {
                self.cursor = idx + 1;
                if act == RoutineAct::Continue {
                    self.run_steps(RunMode::Resume, false).await
                } else {
                    match self.program.next_stop(self.cursor) {
                        Some(_) => RunEnd::Paused(PauseReason::Step),
                        None => RunEnd::Finished,
                    }
                }
            }
            StepRes::Pause(r) => RunEnd::Paused(r),
            StepRes::Stopped => RunEnd::Stopped,
            StepRes::LeaseLost => RunEnd::LeaseLost,
            StepRes::Terminate(e) => RunEnd::Terminate(e),
        }
    }

    async fn run_routine(&mut self, act: RoutineAct) -> RunEnd {
        let start_depth = self
            .routine_ctx
            .as_ref()
            .and_then(|c| c.at.as_ref().map(|a| a.2))
            .unwrap_or(1);
        let Some(r) = self.routine.as_mut() else {
            return self.finish_routine_step(act).await;
        };
        let stop = match act {
            RoutineAct::Into => r.command(RoutineCommand::StepInto).await,
            RoutineAct::Over => r.command(RoutineCommand::StepOver).await,
            RoutineAct::Continue => r.command(RoutineCommand::Continue).await,
            RoutineAct::Out => {
                // simulated: step over until the stack is shallower (cap 10 000)
                let mut last = Ok(RoutineStop::TargetDone);
                for _ in 0..10_000 {
                    last = r.command(RoutineCommand::StepOver).await;
                    match &last {
                        Ok(RoutineStop::At { depth, .. }) if *depth >= start_depth => continue,
                        _ => break,
                    }
                }
                last
            }
        };
        self.handle_routine_stop(stop, act, false).await
    }

    // ---------------------------------------------------------------------------- termination

    async fn restore_idle_timeout(&mut self) {
        if let Some(old) = self.old_idle_timeout.take() {
            let mut sink = StepSink::new(1);
            let _ = self
                .lease
                .execute(
                    QueryRequest::new(format!(
                        "SET idle_in_transaction_session_timeout = '{}'",
                        old.replace('\'', "''")
                    )),
                    &mut sink,
                )
                .await;
        }
    }

    async fn end_transaction(&mut self, end: DebugEndAction) {
        if !self.owned || self.lease.state().transaction == TxStatus::Idle {
            return;
        }
        match end {
            DebugEndAction::Commit => match self.lease.commit().await {
                Ok(()) => {
                    self.committed = true;
                    self.info("debug transaction committed");
                }
                Err(e) => {
                    self.emit(DebugOutput::Error {
                        step: None,
                        error: error_json(&e),
                    });
                    let _ = self.lease.rollback().await;
                    self.rolled_back = true;
                }
            },
            DebugEndAction::Keep => {
                self.owned = false;
                self.info("debug transaction left open on the tab");
                return;
            }
            _ => {
                if self.lease.rollback().await.is_ok() {
                    self.rolled_back = true;
                    self.info("debug transaction rolled back");
                }
            }
        }
        self.owned = false;
        self.tx_since = None;
    }

    async fn abort_routine(&mut self) {
        if let Some(ctx) = self.routine_ctx.take() {
            if let Some(r) = self.routine.as_mut() {
                let _ = r.command(RoutineCommand::Abort).await;
                let _ = r.disarm().await;
            }
            let qid = *self.shared.query.lock().unwrap();
            if let Some(q) = qid {
                let _ = self.lease.cancel(q).await;
            }
            let _ = ctx.handle.await;
            *self.shared.query.lock().unwrap() = None;
        }
    }

    async fn terminate(&mut self, reason: TerminateReason) {
        if self.terminated {
            return;
        }
        self.abort_routine().await;
        if let Some(p) = self.eval_proxy.take() {
            let _ = p.close().await;
        }
        // restoring the server timeout inside a transaction would be undone by its rollback
        if self.lease.state().transaction == TxStatus::Idle && !self.lease.state().broken {
            self.restore_idle_timeout().await;
        }
        let report = DebugEndReport {
            steps_run: self.steps_run,
            committed: self.committed,
            rolled_back: self.rolled_back,
        };
        *self.shared.report.lock().unwrap() = Some(report);
        self.terminated = true;
        self.set_status(DebugStatus::Terminated {
            reason,
            report: Some(report),
        });
        self.state.variables = vec![];
        self.publish();
    }

    /// End of script: ask, or apply the chosen action.
    async fn finish_script(&mut self) {
        let has_tx = self.owned && self.lease.state().transaction != TxStatus::Idle;
        if has_tx {
            match self.request.options.end_action {
                DebugEndAction::Ask => {
                    self.cursor = self.program.steps.len();
                    self.enter_pause(PauseReason::AwaitingEnd).await;
                    return;
                }
                other => self.end_transaction(other).await,
            }
        }
        self.terminate(TerminateReason::Finished).await;
    }

    /// Stop requested. Returns the report when the session ended, `None` when it now waits for
    /// the end decision (`Ask` with an open debug transaction).
    async fn stop_now(&mut self, end: Option<DebugEndAction>) -> Option<DebugEndReport> {
        let awaiting = matches!(
            self.state.status,
            DebugStatus::Paused {
                reason: PauseReason::AwaitingEnd
            }
        );
        let mut end = end.unwrap_or(self.request.options.end_action);
        self.abort_routine().await;
        let has_tx = self.owned && self.lease.state().transaction != TxStatus::Idle;
        if has_tx && end == DebugEndAction::Ask && !awaiting {
            self.enter_pause(PauseReason::AwaitingEnd).await;
            return None;
        }
        if end == DebugEndAction::Ask {
            end = DebugEndAction::Rollback;
        }
        self.end_transaction(end).await;
        self.terminate(TerminateReason::Stopped).await;
        *self.shared.report.lock().unwrap()
    }

    async fn run_end(&mut self, end: RunEnd) {
        match end {
            RunEnd::Paused(r) => self.enter_pause(r).await,
            RunEnd::Finished => self.finish_script().await,
            RunEnd::Stopped => {
                let _ = self.stop_now(None).await;
            }
            RunEnd::LeaseLost => self.terminate(TerminateReason::LeaseLost).await,
            RunEnd::Terminate(e) => {
                self.info(format!("script aborted: {}", e.message()));
                let end = match self.request.options.end_action {
                    DebugEndAction::Ask => DebugEndAction::Rollback,
                    other => other,
                };
                self.end_transaction(end).await;
                self.terminate(TerminateReason::Error).await;
            }
        }
    }

    // ----------------------------------------------------------------------------------- timers

    fn deadline(&self) -> Option<Instant> {
        if !matches!(self.state.status, DebugStatus::Paused { .. }) {
            return None;
        }
        let ttl = Duration::from_secs(u64::from(self.settings.paused_ttl_secs));
        let mut d = self.last_cmd + ttl;
        if self.settings.tx_warn_secs > 0 && !self.warned {
            if let Some(since) = self.tx_since {
                d = d.min(since + Duration::from_secs(u64::from(self.settings.tx_warn_secs)));
            }
        }
        Some(d)
    }

    async fn on_timer(&mut self) {
        let now = Instant::now();
        let ttl = Duration::from_secs(u64::from(self.settings.paused_ttl_secs));
        if now >= self.last_cmd + ttl {
            self.info(
                "paused for too long: the debug session ended and its transaction was rolled back",
            );
            self.abort_routine().await;
            self.end_transaction(DebugEndAction::Rollback).await;
            self.terminate(TerminateReason::Ttl).await;
            return;
        }
        if self.settings.tx_warn_secs > 0 && !self.warned {
            if let Some(since) = self.tx_since {
                if now >= since + Duration::from_secs(u64::from(self.settings.tx_warn_secs)) {
                    self.warned = true;
                    self.info("the transaction has been open for a while: it may hold locks");
                    self.publish();
                }
            }
        }
    }

    // ---------------------------------------------------------------------------- commands

    fn script_mode(&self, action: &DebugAction) -> Result<RunMode> {
        Ok(match action {
            DebugAction::RunToCursor {
                source,
                line,
                force,
            } => {
                if *source != SourceRef::Script {
                    return Err(Error::Invalid(
                        "run to cursor in a routine needs a routine frame".into(),
                    ));
                }
                let target = self
                    .program
                    .step_at_line(*line)
                    .ok_or_else(|| Error::Invalid(format!("no statement at line {line}")))?;
                if target < self.cursor {
                    return Err(Error::Invalid(
                        "the cursor is behind the current statement".into(),
                    ));
                }
                RunMode::RunTo {
                    target,
                    force: *force,
                }
            }
            DebugAction::Resume => RunMode::Resume,
            _ => RunMode::Step,
        })
    }

    /// A command while paused. Returns the run to start, if any.
    async fn handle_paused(&mut self, cmd: Cmd) -> Option<RunRequest> {
        self.last_cmd = Instant::now();
        match cmd {
            Cmd::Act(action, confirm, reply) => match self.begin_action(action, confirm).await {
                Ok(Some(req)) => {
                    self.set_status(DebugStatus::Running {
                        step: self.cursor as u32,
                    });
                    self.children.clear();
                    self.publish();
                    let st = self.state.clone();
                    reply_with(reply, Ok(st));
                    Some(req)
                }
                Ok(None) => {
                    let st = self.state.clone();
                    reply_with(reply, Ok(st));
                    None
                }
                Err(e) => {
                    reply_with(reply, Err(e));
                    None
                }
            },
            Cmd::Stop(end, reply) => {
                let r = self.stop_now(end).await.unwrap_or(DebugEndReport {
                    steps_run: self.steps_run,
                    committed: false,
                    rolled_back: false,
                });
                reply_with(reply, Ok(r));
                None
            }
            Cmd::Evaluate {
                expr,
                frame,
                echo,
                reply,
            } => {
                let _ = frame;
                let prev = self.state.status.clone();
                self.set_status(DebugStatus::Evaluating);
                self.publish();
                let skip = self.eval_skip();
                let result = self.eval_expression(&expr, skip.as_deref()).await;
                if echo {
                    self.emit(DebugOutput::Evaluated {
                        expr: expr.clone(),
                        result: result.clone(),
                    });
                }
                self.set_status(prev);
                self.publish();
                reply_with(reply, Ok(result));
                None
            }
            Cmd::SetBreakpoints(source, bps, reply) => {
                let r = match source {
                    SourceRef::Script => self.apply_breakpoints(bps),
                    SourceRef::Routine { routine } => {
                        self.routine_bps.retain(|b| b.routine != routine);
                        let mut statuses = vec![];
                        for bp in bps {
                            statuses.push(BreakpointStatus {
                                id: bp.id.clone(),
                                verified: self.routine_ok,
                                line: bp.line,
                                message: (!self.routine_ok)
                                    .then(|| "routine debugging is not available".to_string()),
                            });
                            self.routine_bps.push(RoutineBreakpoint {
                                routine: routine.clone(),
                                source_hash: None,
                                breakpoint: bp,
                            });
                        }
                        statuses
                    }
                };
                self.publish();
                reply_with(reply, Ok(r));
                None
            }
            Cmd::SetMuted(m, reply) => {
                self.muted = m;
                self.publish();
                let st = self.state.clone();
                reply_with(reply, Ok(st));
                None
            }
            Cmd::SetVariable(r, v, reply) => {
                let res = self.set_variable(&r, &v).await;
                reply_with(reply, res);
                None
            }
            Cmd::Children(r, reply) => {
                let res = self.load_children(&r).await;
                reply_with(reply, res);
                None
            }
            Cmd::SetWatches(w, reply) => {
                self.watches = w;
                let prev = self.state.status.clone();
                self.set_status(DebugStatus::Evaluating);
                self.publish();
                self.state.watches = self.eval_watches().await;
                self.set_status(prev);
                self.publish();
                let res = self.state.watches.clone();
                reply_with(reply, Ok(res));
                None
            }
            Cmd::SelectFrame(f, reply) => {
                let n = self.state.frames.len() as u32;
                if f >= n {
                    reply_with(reply, Err(Error::NotFound(format!("frame {f}"))));
                } else {
                    if f + 1 < n {
                        if let Some(r) = self.routine.as_mut() {
                            let _ = r.select_frame(f).await;
                        }
                    }
                    self.state.selected_frame = f;
                    self.children.remove("g:Routine");
                    self.publish();
                    let st = self.state.clone();
                    reply_with(reply, Ok(st));
                }
                None
            }
            Cmd::RoutineSource(routine, reply) => {
                let res = match self.routine.as_mut() {
                    Some(r) => r.source(&routine).await,
                    None => Err(Error::Unsupported(
                        "routine debugging is not available".into(),
                    )),
                };
                reply_with(reply, res);
                None
            }
        }
    }

    /// Validate an action and turn it into a run (`None` = handled without running).
    async fn begin_action(
        &mut self,
        action: DebugAction,
        confirm: Option<ConfirmationGrant>,
    ) -> Result<Option<RunRequest>> {
        if matches!(
            self.state.status,
            DebugStatus::Paused {
                reason: PauseReason::AwaitingEnd
            }
        ) {
            return match action {
                DebugAction::Pause => Ok(None),
                _ => Err(Error::Invalid(
                    "the script finished: choose Commit or Rollback".into(),
                )),
            };
        }
        self.shared.pause.store(false, Ordering::SeqCst);
        if self.routine_ctx.is_some() {
            let act = match action {
                DebugAction::StepInto
                | DebugAction::ForceStepInto
                | DebugAction::SmartStepInto { .. } => RoutineAct::Into,
                DebugAction::StepOver => RoutineAct::Over,
                DebugAction::StepOut => RoutineAct::Out,
                DebugAction::Resume => RoutineAct::Continue,
                DebugAction::Pause => return Ok(None),
                DebugAction::RunToCursor { .. } | DebugAction::JumpTo { .. } => {
                    return Err(Error::Unsupported("not available inside a routine".into()));
                }
            };
            return Ok(Some(RunRequest::Routine(act)));
        }
        self.pending_confirm = confirm;
        match action {
            DebugAction::Pause => Ok(None),
            DebugAction::StepOut => Err(Error::Invalid(
                "Step Out is only available inside a routine".into(),
            )),
            DebugAction::JumpTo { line } => {
                let idx = self
                    .program
                    .step_at_line(line)
                    .ok_or_else(|| Error::Invalid(format!("no statement at line {line}")))?;
                self.cursor = idx;
                self.error_pending = false;
                self.enter_pause(PauseReason::Step).await;
                Ok(None)
            }
            DebugAction::StepInto
            | DebugAction::ForceStepInto
            | DebugAction::SmartStepInto { .. } => {
                let force = matches!(action, DebugAction::ForceStepInto);
                let smart = match &action {
                    DebugAction::SmartStepInto { signature } => Some(signature.clone()),
                    _ => None,
                };
                if self.error_pending {
                    self.error_pending = false;
                    self.cursor += 1;
                }
                let step = self.program.steps.get(self.cursor).cloned();
                let mut targets: Vec<RoutineRef> = vec![];
                if let (true, Some(step), Some(r)) =
                    (self.routine_ok, step.as_ref(), self.routine.as_mut())
                {
                    if step.is_stop() {
                        targets = r.step_into_targets(&*self.lease, step, force).await?;
                        if let Some(sig) = &smart {
                            targets.retain(|t| &t.signature == sig);
                        }
                    }
                }
                if targets.is_empty() {
                    let why = if self.routine_ok {
                        "Step Into: the statement calls no routine that can be debugged (running Step Over)"
                    } else {
                        "Step Into is not available: this server has no routine debugger (running Step Over)"
                    };
                    self.info(why);
                    return Ok(Some(RunRequest::Script {
                        mode: RunMode::Step,
                        first: true,
                        into: None,
                    }));
                }
                Ok(Some(RunRequest::Script {
                    mode: RunMode::Step,
                    first: true,
                    into: Some(targets),
                }))
            }
            other => {
                let mode = self.script_mode(&other)?;
                Ok(Some(RunRequest::Script {
                    mode,
                    first: true,
                    into: None,
                }))
            }
        }
    }

    async fn do_run(&mut self, req: RunRequest) -> RunEnd {
        match req {
            RunRequest::Script {
                mode, first, into, ..
            } => match into {
                None => self.run_steps(mode, first).await,
                Some(targets) => {
                    if self.cursor >= self.program.steps.len() {
                        return RunEnd::Finished;
                    }
                    self.run_step_into(targets).await
                }
            },
            RunRequest::Routine(act) => self.run_routine(act).await,
        }
    }
}

/// Run `req` while answering the commands that may arrive during a run (Pause, Stop, snapshots).
async fn run_phase(core: &mut Core, req: RunRequest, rx: &mut mpsc::Receiver<Cmd>) {
    let shared = core.shared.clone();
    let lease = core.lease.clone();
    let mut stops: Vec<(Option<DebugEndAction>, Reply<DebugEndReport>)> = Vec::new();
    let mut dropped = false;
    let end = {
        let run = core.do_run(req);
        tokio::pin!(run);
        loop {
            tokio::select! {
                end = &mut run => break end,
                cmd = rx.recv(), if !dropped => match cmd {
                    None => {
                        dropped = true;
                        shared.stop.store(true, Ordering::SeqCst);
                        let q = *shared.query.lock().unwrap();
                        if let Some(q) = q {
                            let _ = lease.cancel(q).await;
                        }
                    }
                    Some(Cmd::Act(DebugAction::Pause, _, reply)) => {
                        shared.pause.store(true, Ordering::SeqCst);
                        let st = shared.state.lock().unwrap().clone();
                        reply_with(reply, Ok(st));
                    }
                    Some(Cmd::Stop(end, reply)) => {
                        shared.stop.store(true, Ordering::SeqCst);
                        stops.push((end, reply));
                        let q = *shared.query.lock().unwrap();
                        if let Some(q) = q {
                            let _ = lease.cancel(q).await;
                        }
                    }
                    Some(other) => reject_busy(other),
                },
            }
        }
    };
    shared.pause.store(false, Ordering::SeqCst);
    let stop_flag = shared.stop.swap(false, Ordering::SeqCst);
    if dropped || !stops.is_empty() || stop_flag {
        let mut end_action = stops.first().and_then(|s| s.0);
        if dropped && stops.is_empty() {
            end_action = Some(DebugEndAction::Rollback);
        }
        let rep = core.stop_now(end_action).await.unwrap_or(DebugEndReport {
            steps_run: core.steps_run,
            committed: false,
            rolled_back: false,
        });
        for (_, reply) in stops {
            let _ = reply.send(Ok(rep));
        }
        return;
    }
    core.run_end(end).await;
}

fn reject_busy(cmd: Cmd) {
    let busy = || Error::Busy("the debugger is running a statement".into());
    match cmd {
        Cmd::Act(_, _, r) | Cmd::SetMuted(_, r) | Cmd::SelectFrame(_, r) => {
            reply_with(r, Err(busy()))
        }
        Cmd::Stop(_, r) => reply_with(r, Err(busy())),
        Cmd::Evaluate { reply, .. } => reply_with(reply, Err(busy())),
        Cmd::SetBreakpoints(_, _, r) => reply_with(r, Err(busy())),
        Cmd::SetVariable(_, _, r) => reply_with(r, Err(busy())),
        Cmd::Children(_, r) => reply_with(r, Err(busy())),
        Cmd::SetWatches(_, r) => reply_with(r, Err(busy())),
        Cmd::RoutineSource(_, r) => reply_with(r, Err(busy())),
    }
}

async fn actor_main(mut core: Core, mut rx: mpsc::Receiver<Cmd>, initial: Option<RunRequest>) {
    let mut pending = initial;
    loop {
        if let Some(req) = pending.take() {
            run_phase(&mut core, req, &mut rx).await;
            if core.terminated {
                break;
            }
            continue;
        }
        let deadline = core.deadline();
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None => {
                    // every handle is gone: end like a Stop with rollback
                    core.abort_routine().await;
                    core.end_transaction(DebugEndAction::Rollback).await;
                    core.terminate(TerminateReason::SessionClosed).await;
                }
                Some(cmd) => pending = core.handle_paused(cmd).await,
            },
            () = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => core.on_timer().await,
        }
        if core.terminated {
            break;
        }
    }
    // late commands: the session has ended
    rx.close();
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Stop(_, r) => {
                let rep = core.shared.report.lock().unwrap().ok_or_else(ended);
                reply_with(r, rep);
            }
            Cmd::Act(_, _, r) | Cmd::SetMuted(_, r) | Cmd::SelectFrame(_, r) => {
                reply_with(r, Err(ended()))
            }
            Cmd::Evaluate { reply, .. } => reply_with(reply, Err(ended())),
            Cmd::SetBreakpoints(_, _, r) => reply_with(r, Err(ended())),
            Cmd::SetVariable(_, _, r) => reply_with(r, Err(ended())),
            Cmd::Children(_, r) => reply_with(r, Err(ended())),
            Cmd::SetWatches(_, r) => reply_with(r, Err(ended())),
            Cmd::RoutineSource(_, r) => reply_with(r, Err(ended())),
        }
    }
}

/// Take over `lease` and start debugging `request.text`; `routine` is the routine debugger of the
/// server (PostgreSQL `pldbgapi`) when there is one. The first state goes to `sink`.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    profile: DialectProfile,
    lease: Arc<dyn Lease>,
    gate: Arc<dyn StepGate>,
    sink: Arc<dyn DebugSink>,
    proxy: Arc<dyn ProxyOpener>,
    settings: DebugSettings,
    request: DebugStartRequest,
    routine: Option<Box<dyn RoutineDebugger>>,
) -> Result<ActorHandle> {
    let dialect = lease.script_dialect().unwrap_or(profile.dialect);
    let program = DebugProgram::build_with(
        dialect,
        &request.text,
        request.range,
        request.options.tsql_step_unit,
    )?;
    let kit = kit_for(&DialectProfile {
        dialect,
        ..profile.clone()
    });
    let debug_id = Uuid::new_v4();
    let lease_id = request.lease_id;
    let shim = (dialect == ScriptDialect::MsSql
        && request.options.tsql_step_unit == dbstudio_core::debug::TsqlStepUnit::Statement)
        .then(|| TsqlShim::new(&program));
    let lstate = lease.state();
    if lstate.broken {
        return Err(Error::Connection("the lease is broken".into()));
    }
    let muted = request.muted;
    let enabled_bp = request.breakpoints.iter().any(|b| b.enabled) && !muted;
    let stop_on_entry = request.options.stop_on_entry.unwrap_or(!enabled_bp);
    let now = Instant::now();

    let mut old_idle_timeout = None;
    let mut owned = false;
    let mut tx_since = None;
    // Wrap: the debugger opens the transaction when the lease is in auto-commit
    if request.options.tx_mode == DebugTxMode::Wrap
        && lstate.auto_commit
        && profile.caps.manual_transactions
        && lstate.transaction == TxStatus::Idle
    {
        if matches!(profile.kind, DbKind::Postgres | DbKind::YugabyteDb) {
            // server side guard: the server drops an abandoned connection (design 2.6)
            let mut probe = StepSink::new(1);
            if lease
                .execute(
                    QueryRequest::new("SHOW idle_in_transaction_session_timeout"),
                    &mut probe,
                )
                .await
                .is_ok()
            {
                if let Some(Value::Text(t)) = probe
                    .finish()
                    .first()
                    .and_then(|g| g.rows.first())
                    .and_then(|r| r.first())
                {
                    old_idle_timeout = Some(t.clone());
                }
            }
            if old_idle_timeout.is_some() {
                let ms = (u64::from(settings.paused_ttl_secs) + 60) * 1000;
                let mut s = StepSink::new(1);
                let _ = lease
                    .execute(
                        QueryRequest::new(format!(
                            "SET idle_in_transaction_session_timeout = {ms}"
                        )),
                        &mut s,
                    )
                    .await;
            }
        }
        lease.begin(TxOptions::default()).await?;
        owned = true;
        tx_since = Some(now);
    } else if lstate.transaction != TxStatus::Idle {
        tx_since = Some(now);
    }

    let (seq_tx, _) = watch::channel(0u64);
    let initial_state = DebugState {
        debug_id,
        lease_id,
        seq: 0,
        status: DebugStatus::Starting,
        position: None,
        frames: vec![],
        selected_frame: 0,
        variables: vec![],
        watches: vec![],
        inline_values: vec![],
        breakpoints: vec![],
        transaction: DebugTxInfo {
            status: lstate.transaction,
            owned,
            open_for_ms: None,
            warn: false,
            ttl_left_secs: None,
        },
        caps: DebugSessionCaps {
            step_into: false,
            step_out: false,
            pause_running: true,
            set_variable: true,
            routine: false,
            jump_to: true,
        },
        step_into_targets: vec![],
        muted,
    };
    let shared = Arc::new(Shared {
        pause: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        query: Mutex::new(None),
        state: Mutex::new(initial_state.clone()),
        report: Mutex::new(None),
        seq: seq_tx,
    });
    let mut routine = routine;
    let mut routine_ok = false;
    if request.options.routine && profile.caps.routine_debug {
        if let Some(r) = routine.as_mut() {
            routine_ok = matches!(r.support(&*lease).await, Ok(RoutineDebugSupport::Available));
        }
    }
    let first_stop = program.next_stop(0);
    let params = request.params.clone();
    let watches = request.watches.clone();
    let routine_bps = request.routine_breakpoints.clone();
    let bps = request.breakpoints.clone();
    let mut core = Core {
        debug_id,
        profile,
        dialect,
        kit,
        lease,
        gate,
        sink,
        proxy,
        settings,
        request,
        params,
        program,
        shim,
        shared: shared.clone(),
        seq: 0,
        state: initial_state,
        cursor: 0,
        error_pending: false,
        pending_confirm: None,
        bps: vec![],
        routine_bps,
        muted,
        watches,
        owned,
        tx_since,
        warned: false,
        last_cmd: now,
        steps_run: 0,
        committed: false,
        rolled_back: false,
        multi_open: false,
        queued: 0,
        last_result: None,
        inline: vec![],
        children: HashMap::new(),
        seen: HashMap::new(),
        pause_no: 0,
        routine,
        routine_ok,
        routine_ctx: None,
        eval_proxy: None,
        old_idle_timeout,
        terminated: false,
    };
    core.apply_breakpoints(bps);
    core.publish();
    let initial = if first_stop.is_none() {
        // nothing to run: finish at once
        core.finish_script().await;
        None
    } else if stop_on_entry {
        core.enter_pause(PauseReason::Entry).await;
        None
    } else {
        core.set_status(DebugStatus::Running { step: 0 });
        core.publish();
        Some(RunRequest::Script {
            mode: RunMode::Resume,
            first: false,
            into: None,
        })
    };
    let (tx, rx) = mpsc::channel(32);
    let terminated = core.terminated;
    let handle = ActorHandle {
        debug_id,
        lease_id,
        tx,
        shared,
    };
    if !terminated {
        tokio::spawn(actor_main(core, rx, initial));
    }
    Ok(handle)
}
