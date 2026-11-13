import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ipc, toIpcError } from './ipc';
import { mockEmit, resetMockPhase2 } from './mock';
import { ROOT_PATH } from './objectPath';
import type { ExecuteRequest, QueryEvent, SessionInfo } from './types';

async function open(index = 0): Promise<SessionInfo> {
  const snap = await ipc.connectionsList();
  return ipc.sessionOpen(snap.profiles[index]!.id, 'pw');
}

beforeEach(() => resetMockPhase2());

describe('toIpcError with the new kinds', () => {
  it('knows the new kinds and keeps their fields', () => {
    for (const kind of [
      'readOnly',
      'confirmationRequired',
      'busy',
      'permission',
      'driverUnavailable',
      'sshHostKey',
      'integrity',
      'changeFailed',
    ]) {
      expect(toIpcError({ kind, message: 'm' }).kind).toBe(kind);
    }
    const confirmation = {
      token: 't',
      confirmText: 'app',
      statements: [],
      expiresInMs: 1000,
    };
    expect(
      toIpcError({ kind: 'confirmationRequired', message: 'm', confirmation }).confirmation,
    ).toEqual(confirmation);
    expect(toIpcError({ kind: 'changeFailed', message: 'm', index: 2, applied: 1 })).toEqual({
      kind: 'changeFailed',
      message: 'm',
      index: 2,
      applied: 1,
    });
    expect(toIpcError({ kind: 'readOnly', message: 'm', position: 4 }).position).toBe(4);
  });
});

describe('mock sessions and leases', () => {
  it('reports the script dialect of a session', async () => {
    const pg = await open(0);
    const my = await open(1);
    expect(pg.dialect).toEqual({ name: 'postgres' });
    expect(my.dialect.name).toBe('mySql');
  });

  it('runs a script on a lease with statementStart/End events and tracks transactions', async () => {
    const s = await open();
    const lease = await ipc.leaseOpen(s.sessionId, {});
    expect(lease.state.autoCommit).toBe(true);
    const events: string[] = [];
    const request: ExecuteRequest = { text: 'select 😀', queryId: 'q1' };
    const res = await ipc.leaseExecute(lease.leaseId, request, (e: QueryEvent) => {
      events.push(e.type);
      if (e.type === 'statementStart') expect(e.start.range).toEqual({ start: 0, end: 8 });
    });
    expect(events).toEqual(['statementStart', 'columns', 'rows', 'statementEnd']);
    expect(res.summary.statements).toBe(1);
    expect((await ipc.leaseBegin(lease.leaseId, {})).transaction).toBe('active');
    await expect(ipc.leaseSetAutoCommit(lease.leaseId, true)).rejects.toMatchObject({
      kind: 'busy',
    });
    expect((await ipc.leaseRollback(lease.leaseId)).transaction).toBe('idle');
    expect(await ipc.leaseClose(lease.leaseId)).toEqual({ rolledBack: false, forced: false });
    await expect(ipc.leaseState(lease.leaseId)).rejects.toMatchObject({ kind: 'notFound' });
  });

  it('analyzes scripts and runs query_execute', async () => {
    const s = await open();
    const a = await ipc.scriptAnalyze(s.sessionId, 'drop table x');
    expect(a.statements[0]).toMatchObject({ verb: 'DROP', class: { kind: 'ddl' } });
    expect((await ipc.scriptAnalyze(s.sessionId, '  ')).statements).toEqual([]);
    const events: string[] = [];
    const r = await ipc.queryExecute(s.sessionId, { text: 'select 1', queryId: 'q' }, (e) =>
      events.push(e.type),
    );
    expect(r.state).toBeNull();
    expect(events.at(-1)).toBe('statementEnd');
  });
});

describe('mock stores and stubs', () => {
  it('keeps history, saved queries and editor tabs in memory', async () => {
    expect(await ipc.historyList({ limit: 10 })).toEqual([]);
    const folder = { id: 'f', name: 'F', parentId: null, sortOrder: 0 };
    await ipc.savedFolderSave(folder);
    expect((await ipc.savedQueriesList()).folders).toEqual([folder]);
    const tab = {
      id: 't',
      position: 0,
      title: 'Q',
      connectionId: null,
      database: null,
      schema: null,
      text: 'select 1',
      filePath: null,
      cursor: 0,
      autoCommit: true,
    };
    await ipc.editorTabsSet([tab]);
    expect(await ipc.editorTabsGet()).toEqual([tab]);
    expect(await ipc.sqlFileOpen()).toBeNull();
    expect(await ipc.historyClear(null)).toBe(0);
  });

  it('answers the other commands with empty data or `unsupported`', async () => {
    const s = await open();
    const table = { segments: [{ kind: 'table' as const, name: 't' }] };
    expect((await ipc.tableEditability(s.sessionId, table)).editable).toBe(false);
    await expect(
      ipc.changesApply(s.sessionId, { model: 'rows', table, keyColumns: [], changes: [] }),
    ).rejects.toMatchObject({ kind: 'unsupported' });
    await expect(
      ipc.planExplain(s.sessionId, null, { queryId: 'q', text: 'select 1' }),
    ).rejects.toMatchObject({ kind: 'unsupported' });
    expect((await ipc.schemaModel(s.sessionId, { scope: ROOT_PATH })).tables).toEqual([]);
    expect(await ipc.ddlGenerate(s.sessionId, { scope: ROOT_PATH })).toEqual([]);
    expect(await ipc.jobsList()).toEqual([]);
    expect(await ipc.backupJobsList()).toEqual([]);
    expect(await ipc.exportPresetsList()).toEqual([]);
    await expect(ipc.exportStart({ destination: { path: '' } } as never)).rejects.toMatchObject({
      kind: 'permission',
    });
    expect(await ipc.filePickSave('a.csv', [])).toBeNull();
    expect(await ipc.backgroundGet()).toEqual({ closeToTray: 'auto', autostart: false });
    expect((await ipc.driverDiagnostics('oracle')).status.state).toBe('planned');
    await expect(ipc.connectionParseUri('mongodb://x')).rejects.toMatchObject({
      kind: 'unsupported',
    });
  });

  it('pages the object tree', async () => {
    const s = await open();
    const first = await ipc.objectChildrenPage(s.sessionId, { parent: ROOT_PATH, limit: 1 });
    expect(first.nodes).toHaveLength(1);
    expect(first.nextCursor).toBe('1');
    const rest = await ipc.objectChildrenPage(s.sessionId, {
      parent: ROOT_PATH,
      limit: 100,
      cursor: first.nextCursor,
    });
    expect(rest.nextCursor).toBeNull();
    expect(first.approximateTotal).toBe(rest.approximateTotal);
    const filtered = await ipc.objectChildrenPage(s.sessionId, {
      parent: ROOT_PATH,
      limit: 100,
      pattern: 'zzz*',
    });
    expect(filtered.nodes).toEqual([]);
  });
});

describe('ipc.onEvent in the mock transport', () => {
  it('delivers emitted events until unsubscribed', async () => {
    const seen: unknown[] = [];
    const off = await ipc.onEvent('job://progress', (p) => seen.push(p));
    mockEmit('job://progress', { n: 1 });
    mockEmit('lease://state', { n: 2 });
    off();
    mockEmit('job://progress', { n: 3 });
    expect(seen).toEqual([{ n: 1 }]);
  });
});

describe('ipc.onEvent and streaming over the dev bridge', () => {
  beforeEach(() => vi.stubEnv('VITE_IPC_BRIDGE_URL', 'http://127.0.0.1:9999'));
  afterEach(() => {
    vi.unstubAllEnvs();
    vi.unstubAllGlobals();
  });

  const ndjson = (lines: unknown[]) =>
    new Response(lines.map((l) => JSON.stringify(l)).join('\n') + '\n', { status: 200 });

  it('reads GET /events as NDJSON and routes by event name', async () => {
    const fetchMock = vi.fn().mockImplementation(
      async (_url: string, init: { signal?: AbortSignal }) =>
        new Promise<Response>((resolve) => {
          resolve(
            ndjson([
              { event: 'job://progress', payload: { a: 1 } },
              { event: 'lease://state', payload: { b: 2 } },
              { event: 'job://progress', payload: { a: 3 } },
            ]),
          );
          void init;
        }),
    );
    vi.stubGlobal('fetch', fetchMock);
    const seen: unknown[] = [];
    const off = await ipc.onEvent('job://progress', (p) => seen.push(p));
    await vi.waitFor(() => expect(seen).toEqual([{ a: 1 }, { a: 3 }]));
    expect(fetchMock.mock.calls[0]![0]).toBe('http://127.0.0.1:9999/events');
    off();
  });

  it('streams lease_execute and reads its `result` line', async () => {
    const result = { summary: { statements: 1, elapsedMs: 2 }, state: null, historyId: 5 };
    const fetchMock = vi.fn().mockResolvedValue(
      ndjson([
        { type: 'statementStart', start: { index: 0, range: { start: 0, end: 1 } } },
        { type: 'statementEnd', end: { outcome: { kind: 'done' }, elapsedMs: 1 } },
        { type: 'result', result },
      ]),
    );
    vi.stubGlobal('fetch', fetchMock);
    const types: string[] = [];
    await expect(
      ipc.leaseExecute('l1', { text: 'x', queryId: 'q' }, (e) => types.push(e.type)),
    ).resolves.toEqual(result);
    expect(types).toEqual(['statementStart', 'statementEnd']);
    expect(fetchMock.mock.calls[0]![0]).toBe('http://127.0.0.1:9999/ipc/lease_execute');
    expect(JSON.parse(fetchMock.mock.calls[0]![1].body)).toEqual({
      leaseId: 'l1',
      request: { text: 'x', queryId: 'q' },
    });
  });
});
