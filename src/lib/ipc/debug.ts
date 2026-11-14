import { mock } from '../mock';
import type {
  Breakpoint,
  BreakpointStatus,
  ConfirmationGrant,
  DebugAction,
  DebugEndAction,
  DebugEndReport,
  DebugStartRequest,
  DebugState,
  DebugSupport,
  DebugVariable,
  EvalResult,
  RoutineBreakpoint,
  RoutineRef,
  RoutineSource,
  SourceRef,
  Uuid,
  WatchResult,
  WatchSpec,
} from '../types';
import { call } from './core';

/**
 * SQL debugger. Commands answer quickly; progress arrives through the `debug://state` and
 * `debug://output` events (`ipc.onEvent`, payload types in `EventPayloads`).
 */
export const debugApi = {
  /** Cheap capability probe (script / routine debugging) of the lease's server. */
  debugSupport: (leaseId: Uuid): Promise<DebugSupport> =>
    call('debug_support', () => mock.debugSupport(leaseId), { leaseId }),
  /** Take over the lease and start debugging; resolves to the first state. */
  debugStart: (request: DebugStartRequest): Promise<DebugState> =>
    call('debug_start', () => mock.debugStart(request), { request }),
  debugStep: (
    debugId: Uuid,
    action: DebugAction,
    /** Answer to a `confirmation` pause (one-time token). */
    confirm?: ConfirmationGrant | null,
  ): Promise<DebugState> =>
    call('debug_step', () => mock.debugStep(debugId, action), {
      debugId,
      action,
      confirm: confirm ?? null,
    }),
  debugResume: (debugId: Uuid): Promise<DebugState> =>
    call('debug_resume', () => mock.debugStep(debugId, { kind: 'resume' }), { debugId }),
  debugPause: (debugId: Uuid): Promise<DebugState> =>
    call('debug_pause', () => mock.debugStep(debugId, { kind: 'pause' }), { debugId }),
  debugRunToCursor: (
    debugId: Uuid,
    source: SourceRef,
    line: number,
    force: boolean,
  ): Promise<DebugState> =>
    call(
      'debug_run_to_cursor',
      () => mock.debugStep(debugId, { kind: 'runToCursor', source, line, force }),
      { debugId, source, line, force },
    ),
  /** `end` null = the `endAction` chosen at start. Idempotent. */
  debugStop: (debugId: Uuid, end?: DebugEndAction | null): Promise<DebugEndReport> =>
    call('debug_stop', () => mock.debugStop(debugId, end ?? null), { debugId, end: end ?? null }),
  /** Stop + start again with `text`; the new session has a new `debugId`. */
  debugRestart: (debugId: Uuid, text: string, end?: DebugEndAction | null): Promise<DebugState> =>
    call('debug_restart', () => mock.debugRestart(debugId, text, end ?? null), {
      debugId,
      text,
      end: end ?? null,
    }),
  /** Read-only evaluation; `echo` also writes the result to the console. */
  debugEvaluate: (
    debugId: Uuid,
    expr: string,
    frame?: number | null,
    echo?: boolean,
  ): Promise<EvalResult> =>
    call('debug_evaluate', () => mock.debugEvaluate(debugId, expr, frame ?? null), {
      debugId,
      expr,
      frame: frame ?? null,
      echo: echo ?? false,
    }),
  /** Replaces every breakpoint of `source` (like DAP `setBreakpoints`). */
  debugSetBreakpoints: (
    debugId: Uuid,
    source: SourceRef,
    breakpoints: Breakpoint[],
  ): Promise<BreakpointStatus[]> =>
    call('debug_set_breakpoints', () => mock.debugSetBreakpoints(debugId, source, breakpoints), {
      debugId,
      source,
      breakpoints,
    }),
  debugSetMuted: (debugId: Uuid, muted: boolean): Promise<DebugState> =>
    call('debug_set_muted', () => mock.debugSetMuted(debugId, muted), { debugId, muted }),
  debugSetVariable: (debugId: Uuid, reference: string, valueText: string): Promise<DebugVariable> =>
    call('debug_set_variable', () => mock.debugSetVariable(debugId, reference, valueText), {
      debugId,
      reference,
      valueText,
    }),
  debugVariableChildren: (debugId: Uuid, reference: string): Promise<DebugVariable[]> =>
    call('debug_variable_children', () => mock.debugVariableChildren(debugId, reference), {
      debugId,
      reference,
    }),
  debugWatchesSet: (debugId: Uuid, watches: WatchSpec[]): Promise<WatchResult[]> =>
    call('debug_watches_set', () => mock.debugWatchesSet(debugId, watches), { debugId, watches }),
  debugSelectFrame: (debugId: Uuid, frame: number): Promise<DebugState> =>
    call('debug_select_frame', () => mock.debugSelectFrame(debugId, frame), { debugId, frame }),
  /** Routine source; `debugId` null before a session exists (setting breakpoints first). */
  debugRoutineSource: (
    debugId: Uuid | null,
    sessionId: Uuid,
    routine: RoutineRef,
  ): Promise<RoutineSource> =>
    call('debug_routine_source', () => mock.debugRoutineSource(debugId, sessionId, routine), {
      debugId,
      sessionId,
      routine,
    }),
  /** Resynchronise after a gap in `seq`. */
  debugSnapshot: (debugId: Uuid): Promise<DebugState> =>
    call('debug_snapshot', () => mock.debugSnapshot(debugId), { debugId }),
  routineBreakpointsGet: (connectionId: Uuid): Promise<RoutineBreakpoint[]> =>
    call('routine_breakpoints_get', () => mock.routineBreakpointsGet(connectionId), {
      connectionId,
    }),
  routineBreakpointsSet: (connectionId: Uuid, list: RoutineBreakpoint[]): Promise<void> =>
    call('routine_breakpoints_set', () => mock.routineBreakpointsSet(connectionId, list), {
      connectionId,
      list,
    }),
};
