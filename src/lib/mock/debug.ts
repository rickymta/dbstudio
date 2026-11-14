// Minimal SQL debugger mock (Step 0): enough for the UI to compile and for tabs to show a
// paused session. Package E replaces it with a full simulation (statement splitting,
// breakpoints, routine frames, watches, confirmation).

import { mockEmit } from './events';
import { mockLeaseSessionId } from './leases';
import { mockSessionKind } from './sessions';
import { clone, ipcError, newId } from './util';
import type {
  Breakpoint,
  BreakpointStatus,
  DebugAction,
  DebugEndAction,
  DebugEndReport,
  DebugState,
  DebugStartRequest,
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

const sessions = new Map<Uuid, DebugState>();
let routineBreakpoints = new Map<Uuid, RoutineBreakpoint[]>();

export function resetMockDebug(): void {
  sessions.clear();
  routineBreakpoints = new Map();
}

function find(debugId: Uuid): DebugState {
  const s = sessions.get(debugId);
  if (!s) throw ipcError('notFound', `debug session ${debugId}`);
  return s;
}

function publish(state: DebugState): DebugState {
  state.seq += 1;
  sessions.set(state.debugId, state);
  mockEmit('debug://state', clone(state));
  return clone(state);
}

function initialState(request: DebugStartRequest): DebugState {
  return {
    debugId: newId(),
    leaseId: request.leaseId,
    seq: 0,
    status: { kind: 'paused', reason: { kind: 'entry' } },
    position: { source: { kind: 'script' }, line: 1, range: null, step: 0, stepCount: 1 },
    frames: [{ id: 0, kind: 'script', name: 'script.sql:1', source: { kind: 'script' }, line: 1 }],
    selectedFrame: 0,
    variables: [],
    watches: [],
    inlineValues: [],
    breakpoints: [],
    transaction: { status: 'idle', owned: false, warn: false },
    caps: {
      stepInto: false,
      stepOut: false,
      pauseRunning: true,
      setVariable: false,
      routine: false,
      jumpTo: true,
    },
    stepIntoTargets: [],
    muted: request.muted ?? false,
  };
}

function terminate(state: DebugState, reason: 'finished' | 'stopped'): DebugEndReport {
  const report: DebugEndReport = { stepsRun: 0, committed: false, rolledBack: false };
  state.status = { kind: 'terminated', reason, report };
  publish(state);
  return report;
}

export const debugMock = {
  async debugSupport(leaseId: Uuid): Promise<DebugSupport> {
    const sessionId = mockLeaseSessionId(leaseId);
    if (!sessionId) throw ipcError('notFound', `lease ${leaseId}`);
    const kind = mockSessionKind(sessionId);
    const routine: DebugSupport['routine'] =
      kind === 'postgres' || kind === 'yugabytedb'
        ? { kind: 'available' }
        : { kind: 'unsupported', reason: 'no debugger API' };
    return { script: true, routine, transactions: true, scriptVariables: false };
  },
  async debugStart(request: DebugStartRequest): Promise<DebugState> {
    return publish(initialState(request));
  },
  async debugStep(debugId: Uuid, action: DebugAction): Promise<DebugState> {
    const state = find(debugId);
    if (action.kind === 'resume') {
      terminate(state, 'finished');
      return clone(state);
    }
    state.status =
      action.kind === 'pause'
        ? { kind: 'paused', reason: { kind: 'pause' } }
        : { kind: 'paused', reason: { kind: 'step' } };
    return publish(state);
  },
  async debugStop(debugId: Uuid, end?: DebugEndAction | null): Promise<DebugEndReport> {
    void end;
    const state = sessions.get(debugId);
    if (!state) return { stepsRun: 0, committed: false, rolledBack: false };
    return terminate(state, 'stopped');
  },
  async debugRestart(
    debugId: Uuid,
    text: string,
    end?: DebugEndAction | null,
  ): Promise<DebugState> {
    void [text, end];
    const old = find(debugId);
    terminate(old, 'stopped');
    return publish(initialState({ leaseId: old.leaseId, text, muted: old.muted }));
  },
  async debugEvaluate(debugId: Uuid, expr: string, frame?: number | null): Promise<EvalResult> {
    void [expr, frame];
    find(debugId);
    return {
      value: { kind: 'skipped', reason: 'browser mock' },
      context: 'lease',
      elapsedMs: 0,
    };
  },
  async debugSetBreakpoints(
    debugId: Uuid,
    source: SourceRef,
    breakpoints: Breakpoint[],
  ): Promise<BreakpointStatus[]> {
    const state = find(debugId);
    const statuses = breakpoints.map((b) => ({ id: b.id, verified: true, line: b.line }));
    if (source.kind === 'script') state.breakpoints = statuses;
    publish(state);
    return clone(statuses);
  },
  async debugSetMuted(debugId: Uuid, muted: boolean): Promise<DebugState> {
    const state = find(debugId);
    state.muted = muted;
    return publish(state);
  },
  async debugSetVariable(
    debugId: Uuid,
    reference: string,
    valueText: string,
  ): Promise<DebugVariable> {
    void [reference, valueText];
    find(debugId);
    throw ipcError('invalid', 'variable is not editable');
  },
  async debugVariableChildren(debugId: Uuid, reference: string): Promise<DebugVariable[]> {
    void reference;
    find(debugId);
    return [];
  },
  async debugWatchesSet(debugId: Uuid, watches: WatchSpec[]): Promise<WatchResult[]> {
    const state = find(debugId);
    state.watches = watches.map((w) => ({
      id: w.id,
      expr: w.expr,
      result: {
        value: { kind: 'skipped', reason: 'browser mock' },
        context: 'lease',
        elapsedMs: 0,
      },
    }));
    publish(state);
    return clone(state.watches);
  },
  async debugSelectFrame(debugId: Uuid, frame: number): Promise<DebugState> {
    const state = find(debugId);
    state.selectedFrame = frame;
    return publish(state);
  },
  async debugRoutineSource(
    debugId: Uuid | null,
    sessionId: Uuid,
    routine: RoutineRef,
  ): Promise<RoutineSource> {
    void [debugId, sessionId];
    return {
      text: `-- ${routine.signature}\nBEGIN\n  RETURN;\nEND`,
      sourceHash: '0'.repeat(64),
      language: 'plpgsql',
      firstLine: 1,
    };
  },
  async debugSnapshot(debugId: Uuid): Promise<DebugState> {
    return clone(find(debugId));
  },
  async routineBreakpointsGet(connectionId: Uuid): Promise<RoutineBreakpoint[]> {
    return clone(routineBreakpoints.get(connectionId) ?? []);
  },
  async routineBreakpointsSet(connectionId: Uuid, list: RoutineBreakpoint[]): Promise<void> {
    routineBreakpoints.set(connectionId, clone(list));
  },
};
