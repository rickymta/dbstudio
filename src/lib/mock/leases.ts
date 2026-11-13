// Mock leases and script helpers: a lease is an in-memory state object; statements return an
// empty "done" result so the editor placeholder flow can run without a backend.

import type {
  ExecuteRequest,
  ExecuteResult,
  LeaseContext,
  LeaseInfo,
  LeaseOptions,
  LeaseState,
  LeaseCloseReport,
  QueryEvent,
  ScriptAnalysis,
  ScriptDialect,
  TxOptions,
  Uuid,
  Vocabulary,
} from '../types';
import { mockSessionDialect } from './sessions';
import { clone, ipcError, newId } from './util';

const leases = new Map<Uuid, { sessionId: Uuid; state: LeaseState }>();

function find(leaseId: Uuid) {
  const lease = leases.get(leaseId);
  if (!lease) throw ipcError('notFound', `lease ${leaseId}`);
  return lease;
}

const emptyResult = (state: LeaseState | null): ExecuteResult => ({
  summary: { statements: 1, elapsedMs: 0 },
  state: state ? clone(state) : null,
  historyId: null,
});

function run(text: string, onEvent: (e: QueryEvent) => void): void {
  onEvent({
    type: 'statementStart',
    start: { index: 0, range: { start: 0, end: Array.from(text).length } },
  });
  onEvent({ type: 'columns', columns: [] });
  onEvent({
    type: 'statementEnd',
    end: { outcome: { kind: 'done' }, elapsedMs: 0 },
  });
}

export const leasesMock = {
  async scriptAnalyze(sessionId: Uuid, text: string): Promise<ScriptAnalysis> {
    const dialect: ScriptDialect = mockSessionDialect(sessionId);
    const end = Array.from(text).length;
    return {
      dialect,
      statements:
        text.trim() === ''
          ? []
          : [
              {
                index: 0,
                range: { start: 0, end },
                class: { kind: 'unknown' },
                verb: text.trim().split(/\s+/)[0]?.toUpperCase() ?? '',
                destructive: null,
                implicitCommit: false,
                targets: [],
              },
            ],
      params: [],
      problems: [],
    };
  },
  async scriptVocabulary(sessionId: Uuid): Promise<Vocabulary> {
    mockSessionDialect(sessionId);
    return { keywords: ['SELECT', 'FROM', 'WHERE'], functions: [], commands: [] };
  },
  async queryExecute(
    sessionId: Uuid,
    request: ExecuteRequest,
    onEvent: (e: QueryEvent) => void,
  ): Promise<ExecuteResult> {
    mockSessionDialect(sessionId);
    run(request.text, onEvent);
    return emptyResult(null);
  },
  async leaseOpen(sessionId: Uuid, options: LeaseOptions): Promise<LeaseInfo> {
    mockSessionDialect(sessionId);
    const leaseId = newId();
    const state: LeaseState = {
      autoCommit: options.autoCommit ?? true,
      transaction: 'idle',
      running: null,
      pendingResult: null,
      database: options.database ?? null,
      schema: options.schema ?? null,
      broken: false,
    };
    leases.set(leaseId, { sessionId, state });
    return { leaseId, sessionId, state: clone(state) };
  },
  async leaseClose(leaseId: Uuid): Promise<LeaseCloseReport> {
    const lease = leases.get(leaseId);
    leases.delete(leaseId);
    return { rolledBack: lease?.state.transaction === 'active', forced: false };
  },
  async leaseExecute(
    leaseId: Uuid,
    request: ExecuteRequest,
    onEvent: (e: QueryEvent) => void,
  ): Promise<ExecuteResult> {
    const lease = find(leaseId);
    run(request.text, onEvent);
    return emptyResult(lease.state);
  },
  async leaseFetchMore(
    leaseId: Uuid,
    queryId: Uuid,
    maxRows: number,
    batchSize: number | null,
    onEvent: (e: QueryEvent) => void,
  ): Promise<ExecuteResult> {
    void maxRows;
    void batchSize;
    const lease = find(leaseId);
    if (lease.state.pendingResult !== queryId) throw ipcError('notFound', `result ${queryId}`);
    onEvent({
      type: 'statementEnd',
      end: { outcome: { kind: 'rows', rowCount: 0, truncated: false }, elapsedMs: 0 },
    });
    return emptyResult(lease.state);
  },
  async leaseDiscardResult(leaseId: Uuid): Promise<LeaseState> {
    const lease = find(leaseId);
    lease.state.pendingResult = null;
    return clone(lease.state);
  },
  async leaseCancel(leaseId: Uuid, queryId: Uuid): Promise<void> {
    void queryId;
    find(leaseId);
  },
  async leaseBegin(leaseId: Uuid, options: TxOptions): Promise<LeaseState> {
    void options;
    const lease = find(leaseId);
    lease.state.transaction = 'active';
    return clone(lease.state);
  },
  async leaseCommit(leaseId: Uuid): Promise<LeaseState> {
    const lease = find(leaseId);
    lease.state.transaction = 'idle';
    return clone(lease.state);
  },
  async leaseRollback(leaseId: Uuid): Promise<LeaseState> {
    const lease = find(leaseId);
    lease.state.transaction = 'idle';
    return clone(lease.state);
  },
  async leaseSetAutoCommit(leaseId: Uuid, autoCommit: boolean): Promise<LeaseState> {
    const lease = find(leaseId);
    if (autoCommit && lease.state.transaction === 'active') {
      throw ipcError('busy', 'a transaction is open');
    }
    lease.state.autoCommit = autoCommit;
    return clone(lease.state);
  },
  async leaseSetContext(leaseId: Uuid, context: LeaseContext): Promise<LeaseState> {
    const lease = find(leaseId);
    lease.state.database = context.database ?? lease.state.database;
    lease.state.schema = context.schema ?? lease.state.schema;
    return clone(lease.state);
  },
  async leaseState(leaseId: Uuid): Promise<LeaseState> {
    return clone(find(leaseId).state);
  },
};

export function resetMockLeases(): void {
  leases.clear();
}
