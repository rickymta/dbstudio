// SQL debugger contract (dbstudio-core `debug.rs`, dbstudio-store `tabs.rs` / `settings.rs`).
// Lines are 1-based (editor gutter, `pldbgapi`); offsets are Unicode scalar offsets.
// Design: docs/design/sql-debugger.md section 3.

import type { ColumnMeta, ObjectPath, QueryEvent, StatementEnd, Uuid, Value } from './core';
import type { TxStatus } from './lease';
import type { ConfirmationRequest, ParamValue, TextRange } from './script';

/** Rows of a result set (`ResultSet` in Rust). */
export interface ResultSet {
  columns: ColumnMeta[];
  rows: Value[][];
  truncated: boolean;
}

/** Serialised `dbstudio_core::Error` (`{ kind, message, ... }`). */
export type DebugError = { kind: string; message: string } & Record<string, unknown>;

/** Where a line lives: the editor script of the session, or the source of a routine. */
export type SourceRef = { kind: 'script' } | { kind: 'routine'; routine: RoutineRef };

export interface RoutineRef {
  /** Stable identity: PG `oid::regprocedure::text`, e.g. `public.f_tax(numeric)`. */
  signature: string;
  oid?: string | null;
  path?: ObjectPath | null;
}

export interface Breakpoint {
  /** Client generated id (uuid text), stable across edits. */
  id: string;
  /** 1-based line in the source. */
  line: number;
  enabled: boolean;
  /** false = never stop (log only). */
  suspend: boolean;
  condition?: string | null;
  /** Text with `{expr}` placeholders written to the console on every hit. */
  logMessage?: string | null;
  /** Stop on the N-th hit (1 = every hit). */
  hitCount?: number | null;
}

export interface RoutineBreakpoint {
  routine: RoutineRef;
  /** sha256 hex of the routine source when the breakpoint was set. */
  sourceHash?: string | null;
  breakpoint: Breakpoint;
}

export interface BreakpointStatus {
  id: string;
  verified: boolean;
  /** Line after snapping to a statement (script) / executable line (routine). */
  line: number;
  message?: string | null;
}

export type DebugTxMode = 'wrap' | 'lease';
export type DebugEndAction = 'ask' | 'rollback' | 'commit' | 'keep';
export type TsqlStepUnit = 'statement' | 'batch';

export interface DebugOptions {
  /** null/absent = true when no enabled breakpoint exists. */
  stopOnEntry?: boolean | null;
  txMode: DebugTxMode;
  endAction: DebugEndAction;
  breakOnError: boolean;
  statementTimeoutMs?: number | null;
  maxRowsPerStep?: number | null;
  /** Use the routine debugger when the server supports it. */
  routine: boolean;
  tsqlStepUnit: TsqlStepUnit;
  recordHistory: boolean;
}

export const DEFAULT_DEBUG_OPTIONS: DebugOptions = {
  stopOnEntry: null,
  txMode: 'wrap',
  endAction: 'ask',
  breakOnError: true,
  statementTimeoutMs: null,
  maxRowsPerStep: null,
  routine: true,
  tsqlStepUnit: 'statement',
  recordHistory: true,
};

export interface WatchSpec {
  id: string;
  expr: string;
}

export interface DebugStartRequest {
  leaseId: Uuid;
  /** Whole document (snapshot). */
  text: string;
  /** Only the statements intersecting this range (selection), char offsets. */
  range?: TextRange | null;
  params?: ParamValue[];
  breakpoints?: Breakpoint[];
  routineBreakpoints?: RoutineBreakpoint[];
  watches?: WatchSpec[];
  options?: DebugOptions;
  muted?: boolean;
}

export type DebugAction =
  | { kind: 'resume' }
  | { kind: 'stepOver' }
  | { kind: 'stepInto' }
  | { kind: 'forceStepInto' }
  | { kind: 'smartStepInto'; signature: string }
  | { kind: 'stepOut' }
  | { kind: 'runToCursor'; source: SourceRef; line: number; force: boolean }
  | { kind: 'jumpTo'; line: number }
  | { kind: 'pause' };

export type PauseReason =
  | { kind: 'entry' }
  | { kind: 'step' }
  | { kind: 'runToCursor' }
  | { kind: 'pause' }
  | { kind: 'routineEntry' }
  | { kind: 'breakpoint'; ids: string[] }
  /** Statement failed (break on error). */
  | { kind: 'error'; error: DebugError }
  /** The guard refused the statement (read-only). */
  | { kind: 'blocked'; error: DebugError }
  | { kind: 'confirmation'; request: ConfirmationRequest }
  /** End of script with an owned transaction and `endAction = ask`. */
  | { kind: 'awaitingEnd' };

export type TerminateReason =
  'finished' | 'stopped' | 'error' | 'leaseLost' | 'sessionClosed' | 'ttl';

export interface DebugEndReport {
  stepsRun: number;
  committed: boolean;
  rolledBack: boolean;
}

export type DebugStatus =
  | { kind: 'starting' }
  | { kind: 'running'; step: number }
  | { kind: 'paused'; reason: PauseReason }
  | { kind: 'evaluating' }
  | { kind: 'terminated'; reason: TerminateReason; report?: DebugEndReport | null };

export interface DebugPosition {
  source: SourceRef;
  line: number;
  /** Statement range (script) in char offsets; absent in routine sources. */
  range?: TextRange | null;
  /** Script step index / count (always the script frame's). */
  step: number;
  stepCount: number;
}

export type FrameKind = 'script' | 'block' | 'routine';

export interface DebugFrame {
  /** 0 = innermost. */
  id: number;
  kind: FrameKind;
  name: string;
  source: SourceRef;
  line: number;
  language?: string | null;
  args?: string | null;
}

export type VariableKind =
  | 'group'
  | 'parameter'
  | 'scriptVariable'
  | 'session'
  | 'setting'
  | 'tempTable'
  | 'column'
  | 'transaction'
  | 'result'
  | 'routineArgument'
  | 'routineLocal';

export interface DebugVariable {
  /** Opaque handle for `debug_variable_children` / `debug_set_variable`. */
  reference: string;
  name: string;
  kind: VariableKind;
  value?: Value | null;
  /** Short text for the tree; the full value is in `value`. */
  display: string;
  typeName?: string | null;
  editable: boolean;
  hasChildren: boolean;
  /** Value changed by the last step (highlighted like JetBrains). */
  changed: boolean;
}

export type EvalValue =
  | { kind: 'scalar'; value: Value; typeName: string }
  | { kind: 'rows'; result: ResultSet }
  | { kind: 'error'; error: DebugError }
  | { kind: 'skipped'; reason: string };

export type EvalContext = 'lease' | 'proxy' | 'routineVariable';

export interface EvalResult {
  value: EvalValue;
  context: EvalContext;
  elapsedMs: number;
}

export interface WatchResult {
  id: string;
  expr: string;
  result: EvalResult;
}

export type InlineKind = 'result' | 'variable' | 'error';

export interface InlineValue {
  source: SourceRef;
  line: number;
  text: string;
  kind: InlineKind;
}

export interface DebugTxInfo {
  status: TxStatus;
  /** Opened by the debugger (Wrap): the end action applies. */
  owned: boolean;
  openForMs?: number | null;
  /** Open longer than `debugTxWarnSecs` while paused. */
  warn: boolean;
  /** Seconds before the paused TTL ends the session. */
  ttlLeftSecs?: number | null;
}

export interface DebugSessionCaps {
  stepInto: boolean;
  stepOut: boolean;
  pauseRunning: boolean;
  setVariable: boolean;
  routine: boolean;
  jumpTo: boolean;
}

/** Snapshot sent by `debug://state` and returned by most commands. */
export interface DebugState {
  debugId: Uuid;
  leaseId: Uuid;
  /** Monotonic per session; shared with `DebugOutputEvent.seq`. */
  seq: number;
  status: DebugStatus;
  position?: DebugPosition | null;
  frames: DebugFrame[];
  selectedFrame: number;
  /** Top level groups (children through `debug_variable_children`). */
  variables: DebugVariable[];
  watches: WatchResult[];
  inlineValues: InlineValue[];
  breakpoints: BreakpointStatus[];
  transaction: DebugTxInfo;
  caps: DebugSessionCaps;
  /** Routines Smart Step Into can pick for the current statement. */
  stepIntoTargets?: RoutineRef[];
  muted: boolean;
}

export type MessageSource = 'notice' | 'print' | 'dbmsOutput' | 'log' | 'info';

export type DebugOutput =
  | { kind: 'stepStart'; step: number; line: number; text: string }
  | { kind: 'query'; step: number; event: QueryEvent }
  | { kind: 'stepEnd'; step: number; end: StatementEnd; truncatedAt?: number | null }
  | { kind: 'message'; text: string; source: MessageSource }
  | { kind: 'error'; step?: number | null; error: DebugError }
  | { kind: 'evaluated'; expr: string; result: EvalResult };

/** Payload of `debug://output`. */
export interface DebugOutputEvent {
  debugId: Uuid;
  seq: number;
  output: DebugOutput;
}

export type RoutineDebugSupport =
  | { kind: 'available' }
  /** Extension available on the server but not created in this database. */
  | { kind: 'needsExtension'; statement: string }
  /** `plugin_debugger` missing from `shared_preload_libraries` (server restart needed). */
  | { kind: 'needsServerConfig'; hint: string }
  | { kind: 'noPermission'; hint: string }
  | { kind: 'unsupported'; reason: string };

export interface DebugSupport {
  script: boolean;
  routine: RoutineDebugSupport;
  transactions: boolean;
  /** Script variable syntax recognised for this dialect (`@x`, `DECLARE @x`...). */
  scriptVariables: boolean;
}

/** Result of `debug_routine_source`. */
export interface RoutineSource {
  text: string;
  sourceHash: string;
  language: string;
  /** Line of the file that `text` line 1 maps to (usually 1). */
  firstLine: number;
}

export type DebugKeymap = 'jetBrains' | 'vsCode';

/** Debugger state saved with an editor tab (`EditorTabState.debug`). */
export interface TabDebugState {
  breakpoints: Breakpoint[];
  watches: WatchSpec[];
  muted: boolean;
}
