//! SQL debugger contract types (script debugger + routine debugger).
//!
//! Everything here crosses the IPC boundary (serde camelCase) and has fixtures in
//! `contract/fixtures/debug.json`. Lines are 1-based (they match the editor gutter and
//! `pldbgapi`); offsets are Unicode scalar offsets. See `docs/design/sql-debugger.md` section 3.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    ConfirmationRequest, ObjectPath, ParamValue, QueryEvent, ResultSet, StatementEnd, TextRange,
    TxStatus, Value,
};

/// Where a line lives: the editor script of the session, or the source of a routine.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SourceRef {
    Script,
    #[serde(rename_all = "camelCase")]
    Routine {
        routine: RoutineRef,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutineRef {
    /// Stable identity: PG `oid::regprocedure::text`, e.g. `public.f_tax(numeric)`.
    pub signature: String,
    /// Server object id when known (PG oid as text); refreshed at every start.
    #[serde(default)]
    pub oid: Option<String>,
    /// Tree path of the routine (explorer, DDL).
    #[serde(default)]
    pub path: Option<ObjectPath>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Breakpoint {
    /// Client generated id (uuid text), stable across edits.
    pub id: String,
    /// 1-based line in the source (script text or routine `prosrc`).
    pub line: u32,
    #[serde(default = "crate::script::yes")]
    pub enabled: bool,
    /// false = never stop (log only).
    #[serde(default = "crate::script::yes")]
    pub suspend: bool,
    #[serde(default)]
    pub condition: Option<String>,
    /// Text with `{expr}` placeholders written to the console on every hit.
    #[serde(default)]
    pub log_message: Option<String>,
    /// Stop on the N-th hit (1 = every hit).
    #[serde(default)]
    pub hit_count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutineBreakpoint {
    pub routine: RoutineRef,
    /// sha256 hex of the routine source when the breakpoint was set.
    #[serde(default)]
    pub source_hash: Option<String>,
    pub breakpoint: Breakpoint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakpointStatus {
    pub id: String,
    pub verified: bool,
    /// Line after snapping to a statement (script) / executable line (routine).
    pub line: u32,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DebugTxMode {
    #[default]
    Wrap,
    Lease,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DebugEndAction {
    #[default]
    Ask,
    Rollback,
    Commit,
    Keep,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TsqlStepUnit {
    #[default]
    Statement,
    Batch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugOptions {
    /// None = true when no enabled breakpoint exists.
    #[serde(default)]
    pub stop_on_entry: Option<bool>,
    #[serde(default)]
    pub tx_mode: DebugTxMode,
    #[serde(default)]
    pub end_action: DebugEndAction,
    #[serde(default = "crate::script::yes")]
    pub break_on_error: bool,
    /// Per statement; None = settings `queryTimeoutMs`.
    #[serde(default)]
    pub statement_timeout_ms: Option<u64>,
    /// None = settings `debugMaxRowsPerStep`.
    #[serde(default)]
    pub max_rows_per_step: Option<u32>,
    /// Use the routine debugger when the server supports it.
    #[serde(default = "crate::script::yes")]
    pub routine: bool,
    #[serde(default)]
    pub tsql_step_unit: TsqlStepUnit,
    #[serde(default = "crate::script::yes")]
    pub record_history: bool,
}

impl Default for DebugOptions {
    fn default() -> Self {
        DebugOptions {
            stop_on_entry: None,
            tx_mode: DebugTxMode::default(),
            end_action: DebugEndAction::default(),
            break_on_error: true,
            statement_timeout_ms: None,
            max_rows_per_step: None,
            routine: true,
            tsql_step_unit: TsqlStepUnit::default(),
            record_history: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchSpec {
    pub id: String,
    pub expr: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugStartRequest {
    pub lease_id: Uuid,
    /// Whole document (snapshot).
    pub text: String,
    /// Only the statements intersecting this range (selection), char offsets.
    #[serde(default)]
    pub range: Option<TextRange>,
    #[serde(default)]
    pub params: Vec<ParamValue>,
    #[serde(default)]
    pub breakpoints: Vec<Breakpoint>,
    #[serde(default)]
    pub routine_breakpoints: Vec<RoutineBreakpoint>,
    #[serde(default)]
    pub watches: Vec<WatchSpec>,
    #[serde(default)]
    pub options: DebugOptions,
    #[serde(default)]
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DebugAction {
    Resume,
    StepOver,
    StepInto,
    ForceStepInto,
    /// Smart Step Into: one routine among `DebugState.stepIntoTargets`.
    #[serde(rename_all = "camelCase")]
    SmartStepInto {
        signature: String,
    },
    StepOut,
    #[serde(rename_all = "camelCase")]
    RunToCursor {
        source: SourceRef,
        line: u32,
        force: bool,
    },
    #[serde(rename_all = "camelCase")]
    JumpTo {
        line: u32,
    },
    Pause,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PauseReason {
    Entry,
    Step,
    RunToCursor,
    Pause,
    RoutineEntry,
    #[serde(rename_all = "camelCase")]
    Breakpoint {
        ids: Vec<String>,
    },
    /// Statement failed (break on error). `error` = serialized `Error`.
    #[serde(rename_all = "camelCase")]
    Error {
        error: serde_json::Value,
    },
    /// The guard refused the statement (read-only).
    #[serde(rename_all = "camelCase")]
    Blocked {
        error: serde_json::Value,
    },
    #[serde(rename_all = "camelCase")]
    Confirmation {
        request: ConfirmationRequest,
    },
    /// End of script with an owned transaction and `endAction = ask`.
    AwaitingEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminateReason {
    Finished,
    Stopped,
    Error,
    LeaseLost,
    SessionClosed,
    Ttl,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DebugStatus {
    Starting,
    #[serde(rename_all = "camelCase")]
    Running {
        step: u32,
    },
    #[serde(rename_all = "camelCase")]
    Paused {
        reason: PauseReason,
    },
    Evaluating,
    #[serde(rename_all = "camelCase")]
    Terminated {
        reason: TerminateReason,
        report: Option<DebugEndReport>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugPosition {
    pub source: SourceRef,
    pub line: u32,
    /// Statement range (script) in char offsets; None in routine sources.
    #[serde(default)]
    pub range: Option<TextRange>,
    /// Script step index / count (always the script frame's).
    pub step: u32,
    pub step_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FrameKind {
    Script,
    Block,
    Routine,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugFrame {
    /// 0 = innermost.
    pub id: u32,
    pub kind: FrameKind,
    /// `f_tax(numeric):12` / `script.sql:8 — UPDATE (3/12)` / `DO block`.
    pub name: String,
    pub source: SourceRef,
    pub line: u32,
    #[serde(default)]
    pub language: Option<String>, // "plpgsql"
    #[serde(default)]
    pub args: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VariableKind {
    Group,
    Parameter,
    ScriptVariable,
    Session,
    Setting,
    TempTable,
    Column,
    Transaction,
    Result,
    RoutineArgument,
    RoutineLocal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugVariable {
    /// Opaque handle for `debug_variable_children` / `debug_set_variable`.
    pub reference: String,
    pub name: String,
    pub kind: VariableKind,
    #[serde(default)]
    pub value: Option<Value>,
    /// Short text for the tree (`'abc'`, `42`, `{3 rows}`); full value in `value`.
    pub display: String,
    #[serde(default)]
    pub type_name: Option<String>,
    #[serde(default)]
    pub editable: bool,
    #[serde(default)]
    pub has_children: bool,
    /// Value changed by the last step (shown highlighted, like JetBrains).
    #[serde(default)]
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EvalValue {
    #[serde(rename_all = "camelCase")]
    Scalar {
        value: Value,
        type_name: String,
    },
    #[serde(rename_all = "camelCase")]
    Rows {
        result: ResultSet,
    },
    #[serde(rename_all = "camelCase")]
    Error {
        error: serde_json::Value,
    },
    Skipped {
        reason: String,
    }, // MULTI open, failed tx, ROW_COUNT preserved...
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EvalContext {
    Lease,
    Proxy,
    RoutineVariable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalResult {
    pub value: EvalValue,
    pub context: EvalContext,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchResult {
    pub id: String,
    pub expr: String,
    pub result: EvalResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InlineKind {
    Result,
    Variable,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineValue {
    pub source: SourceRef,
    pub line: u32,
    pub text: String,
    pub kind: InlineKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugTxInfo {
    pub status: TxStatus,
    /// Opened by the debugger (Wrap): end action applies.
    pub owned: bool,
    #[serde(default)]
    pub open_for_ms: Option<u64>,
    /// Open longer than `debugTxWarnSecs` while paused.
    pub warn: bool,
    /// Seconds before the paused TTL ends the session.
    #[serde(default)]
    pub ttl_left_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugSessionCaps {
    pub step_into: bool,
    pub step_out: bool,
    pub pause_running: bool,
    pub set_variable: bool,
    pub routine: bool,
    pub jump_to: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugState {
    pub debug_id: Uuid,
    pub lease_id: Uuid,
    /// Monotonic per session; shared with `DebugOutputEvent.seq`.
    pub seq: u64,
    pub status: DebugStatus,
    #[serde(default)]
    pub position: Option<DebugPosition>,
    pub frames: Vec<DebugFrame>,
    pub selected_frame: u32,
    /// Top level groups (children via `debug_variable_children`).
    pub variables: Vec<DebugVariable>,
    pub watches: Vec<WatchResult>,
    pub inline_values: Vec<InlineValue>,
    pub breakpoints: Vec<BreakpointStatus>,
    pub transaction: DebugTxInfo,
    pub caps: DebugSessionCaps,
    /// Routines Smart Step Into can pick for the current statement.
    #[serde(default)]
    pub step_into_targets: Vec<RoutineRef>,
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DebugOutput {
    #[serde(rename_all = "camelCase")]
    StepStart { step: u32, line: u32, text: String },
    #[serde(rename_all = "camelCase")]
    Query { step: u32, event: QueryEvent },
    #[serde(rename_all = "camelCase")]
    StepEnd {
        step: u32,
        end: StatementEnd,
        truncated_at: Option<u32>,
    },
    #[serde(rename_all = "camelCase")]
    Message { text: String, source: MessageSource },
    #[serde(rename_all = "camelCase")]
    Error {
        step: Option<u32>,
        error: serde_json::Value,
    },
    #[serde(rename_all = "camelCase")]
    Evaluated { expr: String, result: EvalResult },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageSource {
    Notice,
    Print,
    DbmsOutput,
    Log,
    Info,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugOutputEvent {
    pub debug_id: Uuid,
    pub seq: u64,
    pub output: DebugOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugEndReport {
    pub steps_run: u32,
    pub committed: bool,
    pub rolled_back: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RoutineDebugSupport {
    Available,
    /// Extension available on the server but not created in this database.
    #[serde(rename_all = "camelCase")]
    NeedsExtension {
        statement: String,
    },
    /// `plugin_debugger` missing from `shared_preload_libraries` (server restart needed).
    #[serde(rename_all = "camelCase")]
    NeedsServerConfig {
        hint: String,
    },
    #[serde(rename_all = "camelCase")]
    NoPermission {
        hint: String,
    },
    #[serde(rename_all = "camelCase")]
    Unsupported {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugSupport {
    pub script: bool,
    pub routine: RoutineDebugSupport,
    pub transactions: bool,
    /// Script variable syntax recognised for this dialect (`@x`, `DECLARE @x`...).
    pub script_variables: bool,
}

/// Source of a routine fetched by `debug_routine_source`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutineSource {
    /// Routine body text (`prosrc`).
    pub text: String,
    /// sha256 hex of `text`.
    pub source_hash: String,
    /// Language name (`plpgsql`).
    pub language: String,
    /// Line of the file that `text` line 1 maps to (usually 1).
    pub first_line: u32,
}
