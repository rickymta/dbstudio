//! SQL debugger engine: runs the script of an editor tab one statement at a time on its lease
//! (all dialects) and, where the server has a debug API, steps into stored routines
//! (PostgreSQL `pldbgapi`). Pure Rust + tokio; the host (`src-tauri`) injects the guard, the
//! event sink and the proxy-connection opener through the traits below.
//!
//! [`DebugEngine::start`] spawns the session actor (`actor`); `routine/pg.rs` is the PostgreSQL
//! routine debugger. Design: `docs/design/sql-debugger.md` section 3.6.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use dbstudio_core::debug::{
    Breakpoint, BreakpointStatus, DebugAction, DebugEndAction, DebugEndReport, DebugFrame,
    DebugOutputEvent, DebugStartRequest, DebugState, DebugVariable, EvalResult, RoutineBreakpoint,
    RoutineDebugSupport, RoutineRef, RoutineSource, SourceRef, WatchResult, WatchSpec,
};
use dbstudio_core::{
    Capabilities, ConfirmationGrant, DbKind, Lease, ParamValue, Result, ScriptAnalysis,
    ScriptDialect, TxStatus,
};
use uuid::Uuid;

pub mod actor;
pub mod dialect;
pub mod eval;
pub mod inline;
pub mod program;
pub mod routine;
pub mod tsql_shim;

pub use program::{DebugProgram, Step, StepKind, wrap_single};

/// Facts about the server the engine adapts to (from `SessionInfo` / `ServerInfo.extra["flavor"]`).
#[derive(Debug, Clone)]
pub struct DialectProfile {
    /// Database product.
    pub kind: DbKind,
    /// Flavour inside a product family (`cockroachdb`, `mariadb`, ...).
    pub flavor: Option<String>,
    /// Server version text.
    pub version: Option<String>,
    /// Lexer dialect of the lease.
    pub dialect: ScriptDialect,
    /// Capabilities of the session.
    pub caps: Capabilities,
}

/// Limits and timings taken from the application settings at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugSettings {
    /// `debugTxWarnSecs`; 0 = no warning.
    pub tx_warn_secs: u32,
    /// `debugPausedTtlSecs`.
    pub paused_ttl_secs: u32,
    /// `debugWatchTimeoutMs`.
    pub watch_timeout_ms: u64,
    /// `debugMaxRowsPerStep`.
    pub max_rows_per_step: u32,
    /// `debugInlineValues`.
    pub inline_values: bool,
    /// `queryTimeoutMs` (0 = none), the default statement timeout.
    pub query_timeout_ms: u64,
}

impl Default for DebugSettings {
    fn default() -> Self {
        DebugSettings {
            tx_warn_secs: 60,
            paused_ttl_secs: 900,
            watch_timeout_ms: 5_000,
            max_rows_per_step: 200,
            inline_values: true,
            query_timeout_ms: 0,
        }
    }
}

/// One step as the host guard sees it.
#[derive(Debug)]
pub struct GateRequest<'a> {
    /// Lexer dialect of the lease.
    pub dialect: ScriptDialect,
    /// Step text, not yet bound.
    pub text: &'a str,
    /// Bind parameters of the session.
    pub params: &'a [ParamValue],
    /// Confirmation of a destructive statement (one-time token).
    pub confirm: Option<&'a ConfirmationGrant>,
    /// Current database of the lease when the step runs.
    pub database: Option<&'a str>,
    /// Current schema of the lease.
    pub schema: Option<&'a str>,
}

/// A step that passed the guard: the text to send and how to map errors back.
#[derive(Debug)]
pub struct GatedText {
    /// Bound text.
    pub text: String,
    /// Maps bound offsets to the original step text.
    pub map: dbstudio_sql::OffsetMap,
    /// Analysis of the text that was checked.
    pub analysis: ScriptAnalysis,
}

/// A step that ran, for the query history (the host redacts it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRecord {
    /// Lexer dialect.
    pub dialect: ScriptDialect,
    /// Text that ran (bound).
    pub text: String,
    /// Database at the time.
    pub database: Option<String>,
    /// Elapsed milliseconds.
    pub elapsed_ms: u64,
    /// Rows returned or affected.
    pub rows: Option<u64>,
    /// Error message when the step failed.
    pub error: Option<String>,
}

/// Guard of the host (`src-tauri` implements it with `guard::run_guard`).
pub trait StepGate: Send + Sync {
    /// Bind + guard one step; returns the text to send (bound) and its offset map.
    fn check(&self, req: GateRequest<'_>) -> Result<GatedText>;
    /// Same with `read_only` forced, production off, exactly one Read/Explain statement.
    fn check_read_only(&self, dialect: ScriptDialect, text: &str) -> Result<GatedText>;
    /// Record a step in the query history (redacted by the host).
    fn record(&self, step: &StepRecord);
}

/// Where the engine sends state and console output (`src-tauri`: the event hub).
pub trait DebugSink: Send + Sync {
    /// A new state snapshot (`debug://state`).
    fn state(&self, state: &DebugState);
    /// One console entry (`debug://output`).
    fn output(&self, event: DebugOutputEvent);
}

/// Opens the proxy connection of a routine debugger (`src-tauri`: `Session::open_lease` +
/// `LeaseManager`).
#[async_trait]
pub trait ProxyOpener: Send + Sync {
    /// A new lease on the same server (same TLS/SSH), on `database` when given.
    async fn open_proxy(&self, database: Option<String>) -> Result<Arc<dyn Lease>>;
}

/// How a read-only evaluation is wrapped (design section 2.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalFrame {
    /// Statements run before the expression (`BEGIN READ ONLY`, `SAVEPOINT __dbg`).
    pub before: Vec<String>,
    /// Statements run after it, even when the expression failed (`ROLLBACK`).
    pub after: Vec<String>,
    /// Set when no evaluation is possible (failed transaction, `MULTI` open): the reason.
    pub skip: Option<String>,
}

/// One probe query of the Variables panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// Name of the Variables group the rows belong to.
    pub group: String,
    /// Read-only SQL.
    pub sql: String,
    /// Load without waiting for the user to open the group.
    pub always: bool,
}

/// A script variable found in the program (`@x`, `DECLARE @x`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptVar {
    /// Name with its sigil.
    pub name: String,
    /// Declared type when the syntax has one.
    pub declared_type: Option<String>,
    /// First line where it appears (1-based).
    pub line: u32,
}

/// Dialect specific pieces of the script debugger (one impl per dialect family, in `dialect/*`).
pub trait DialectKit: Send + Sync {
    /// Text that makes the driver's splitter see exactly one statement.
    fn wrap_single(&self, step_text: &str) -> String;
    /// Wrap of a read-only evaluation for the given transaction state.
    fn eval_frame(&self, tx: TxStatus) -> EvalFrame;
    /// Query that evaluates a breakpoint condition to a boolean.
    fn condition_query(&self, cond: &str) -> String;
    /// Probe queries of the Variables panel.
    fn probes(&self) -> Vec<Probe>;
    /// Script variables of `program`.
    fn script_variables(&self, program: &DebugProgram) -> Vec<ScriptVar>;
    /// Statement that assigns `value_text` to `var`.
    fn set_variable_sql(&self, var: &ScriptVar, value_text: &str) -> Result<String>;
    /// MySQL: the next step reads `ROW_COUNT()` / `FOUND_ROWS()`, so probes must not run.
    fn preserves_counters(&self, next_step: &Step) -> bool;
}

/// Where a routine stopped (the answer of every [`RoutineDebugger`] command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutineStop {
    /// Stopped at `line` of `routine`.
    At {
        /// Routine that is running.
        routine: RoutineRef,
        /// 1-based line in its source.
        line: u32,
        /// Stack depth (1 = outermost routine).
        depth: u32,
    },
    /// The statement of the target finished (the routine returned).
    TargetDone,
    /// The debug session was aborted.
    Aborted,
}

/// Command to a running routine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineCommand {
    /// Step into the next call.
    StepInto,
    /// Step over the current line.
    StepOver,
    /// Continue to the next breakpoint.
    Continue,
    /// Abort the target.
    Abort,
}

/// Routine debugger of a server (PostgreSQL `pldbgapi`; Oracle later).
#[async_trait]
pub trait RoutineDebugger: Send {
    /// Can this server debug routines, and if not, why.
    async fn support(&mut self, target: &dyn Lease) -> Result<RoutineDebugSupport>;
    /// Routines the statement `step` calls (Smart Step Into candidates).
    async fn step_into_targets(
        &mut self,
        target: &dyn Lease,
        step: &Step,
        force: bool,
    ) -> Result<Vec<RoutineRef>>;
    /// Arm breakpoints (user + entry of `targets`) for the next statement on the target.
    async fn arm(
        &mut self,
        targets: &[RoutineRef],
        bps: &[RoutineBreakpoint],
    ) -> Result<Vec<BreakpointStatus>>;
    /// Race: target statement finished vs routine stopped.
    async fn wait(
        &mut self,
        target_done: &mut (dyn Future<Output = ()> + Send + Unpin),
    ) -> Result<RoutineStop>;
    /// Step / continue / abort. `target_done` resolves when the target statement ended; the call
    /// must race against it (a step at the last line of the outermost routine would otherwise
    /// block until the server gives up).
    async fn command(
        &mut self,
        cmd: RoutineCommand,
        target_done: &mut (dyn Future<Output = ()> + Send + Unpin),
    ) -> Result<RoutineStop>;
    /// Call stack, innermost first.
    async fn frames(&mut self) -> Result<Vec<DebugFrame>>;
    /// Select the frame Variables and Evaluate refer to.
    async fn select_frame(&mut self, frame: u32) -> Result<()>;
    /// Arguments and locals of the selected frame.
    async fn variables(&mut self) -> Result<Vec<DebugVariable>>;
    /// Change a variable of the selected frame.
    async fn set_variable(&mut self, reference: &str, value_text: &str) -> Result<DebugVariable>;
    /// Source of a routine.
    async fn source(&mut self, routine: &RoutineRef) -> Result<RoutineSource>;
    /// Remove every breakpoint and listener.
    async fn disarm(&mut self) -> Result<()>;
}

/// Entry point: starts one debug session.
#[derive(Debug, Clone, Copy)]
pub struct DebugEngine;

impl DebugEngine {
    /// Take over `lease` and start debugging `request.text`. Returns the client of the session
    /// actor; the first state goes to `sink`. PostgreSQL and YugabyteDB get a routine debugger
    /// (`pldbgapi`) on a hidden proxy connection of the same server; other dialects debug the
    /// script only.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        profile: DialectProfile,
        lease: Arc<dyn Lease>,
        gate: Arc<dyn StepGate>,
        sink: Arc<dyn DebugSink>,
        proxy: Arc<dyn ProxyOpener>,
        settings: DebugSettings,
        request: DebugStartRequest,
    ) -> Result<DebugHandle> {
        let routine: Option<Box<dyn RoutineDebugger>> = match profile.kind {
            DbKind::Postgres | DbKind::YugabyteDb => {
                let database = lease.state().database;
                Some(Box::new(routine::pg::PgRoutineDebugger::new(
                    proxy.clone(),
                    database,
                )))
            }
            _ => None,
        };
        let inner = actor::start(
            profile, lease, gate, sink, proxy, settings, request, routine,
        )
        .await?;
        Ok(DebugHandle { inner })
    }
}

/// Client of one session actor (the actor's handle; every call is forwarded).
#[derive(Debug)]
pub struct DebugHandle {
    inner: actor::ActorHandle,
}

impl DebugHandle {
    /// Id of the session.
    pub fn debug_id(&self) -> Uuid {
        self.inner.debug_id()
    }

    /// Lease the session holds.
    pub fn lease_id(&self) -> Uuid {
        self.inner.lease_id()
    }

    /// False once the session actor is gone (it ended, or panicked).
    pub fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }

    /// Latest state snapshot.
    pub async fn snapshot(&self) -> Result<DebugState> {
        self.inner.snapshot().await
    }

    /// Resume / step / run to cursor / jump / pause; returns the `Running` state at once.
    pub async fn act(
        &self,
        action: DebugAction,
        confirm: Option<ConfirmationGrant>,
    ) -> Result<DebugState> {
        self.inner.act(action, confirm).await
    }

    /// End the session; `None` = the `endAction` chosen at start. Idempotent.
    pub async fn stop(&self, end: Option<DebugEndAction>) -> Result<DebugEndReport> {
        self.inner.stop(end).await
    }

    /// Evaluate an expression in the read-only frame of `frame` (default: selected).
    pub async fn evaluate(&self, expr: &str, frame: Option<u32>, echo: bool) -> Result<EvalResult> {
        self.inner.evaluate(expr, frame, echo).await
    }

    /// Replace the breakpoints of one source.
    pub async fn set_breakpoints(
        &self,
        source: SourceRef,
        breakpoints: Vec<Breakpoint>,
    ) -> Result<Vec<BreakpointStatus>> {
        self.inner.set_breakpoints(source, breakpoints).await
    }

    /// Mute / unmute all breakpoints.
    pub async fn set_muted(&self, muted: bool) -> Result<DebugState> {
        self.inner.set_muted(muted).await
    }

    /// Change a variable.
    pub async fn set_variable(&self, reference: &str, value_text: &str) -> Result<DebugVariable> {
        self.inner.set_variable(reference, value_text).await
    }

    /// Children of a variable (lazy tree).
    pub async fn variable_children(&self, reference: &str) -> Result<Vec<DebugVariable>> {
        self.inner.variable_children(reference).await
    }

    /// Replace the watches and evaluate them when paused.
    pub async fn set_watches(&self, watches: Vec<WatchSpec>) -> Result<Vec<WatchResult>> {
        self.inner.set_watches(watches).await
    }

    /// Select a call-stack frame.
    pub async fn select_frame(&self, frame: u32) -> Result<DebugState> {
        self.inner.select_frame(frame).await
    }

    /// Source of a routine through the session's proxy.
    pub async fn routine_source(&self, routine: &RoutineRef) -> Result<RoutineSource> {
        self.inner.routine_source(routine).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_match_the_application_defaults() {
        let s = DebugSettings::default();
        assert_eq!(
            (
                s.tx_warn_secs,
                s.paused_ttl_secs,
                s.watch_timeout_ms,
                s.max_rows_per_step
            ),
            (60, 900, 5_000, 200)
        );
    }
}
