import { beforeEach, describe, expect, it, vi } from 'vitest';
import type {
  ColumnMeta,
  ExecSummary,
  IpcError,
  QueryEvent,
  TableDataRequest,
  Value,
} from '@/lib/types';
import { ipc } from '@/lib/ipc';
import { CANCEL_RETRY_DELAY_MS, GridModel } from './gridModel';
import { emptyDraft } from './request';

vi.mock('@/lib/ipc', () => ({
  ipc: {
    tableFetch: vi.fn(),
    tableCount: vi.fn(),
    queryCancel: vi.fn(() => Promise.resolve()),
  },
}));

interface FetchCall {
  request: TableDataRequest;
  emit: (e: QueryEvent) => void;
  resolve: () => void;
  reject: (e: IpcError) => void;
}

let calls: FetchCall[] = [];

beforeEach(() => {
  calls = [];
  vi.mocked(ipc.queryCancel).mockClear();
  vi.mocked(ipc.tableCount).mockReset();
  vi.mocked(ipc.tableFetch).mockReset();
  vi.mocked(ipc.tableFetch).mockImplementation((_s, request, onEvent) => {
    return new Promise<ExecSummary>((resolve, reject) => {
      calls.push({
        request,
        emit: onEvent,
        resolve: () => resolve({ statements: 1, elapsedMs: 1 }),
        reject,
      });
    });
  });
});

const COLUMNS: ColumnMeta[] = [{ name: 'id', typeName: 'int', valueKind: 'int' }];
const rows = (from: number, n: number): Value[][] =>
  Array.from({ length: n }, (_, i) => [{ t: 'int', v: from + i } as Value]);
const flush = () => new Promise((r) => setTimeout(r, 0));

// Synchronous scheduler: row batches notify immediately.
const sync = { request: (cb: () => void) => (cb(), 0), cancel: () => undefined };

function makeModel(opts: { pageSize?: number; maxRows?: number } = {}) {
  return new GridModel(
    { sessionId: 's1', table: { segments: [{ kind: 'table', name: 't' }] }, ...opts },
    sync,
  );
}

function feed(call: FetchCall, from: number, n: number, batch = n) {
  call.emit({ type: 'columns', columns: COLUMNS });
  for (let i = 0; i < n; i += batch) {
    call.emit({ type: 'rows', rows: rows(from + i, Math.min(batch, n - i)) });
  }
}

describe('GridModel paging', () => {
  it('appends row batches as they arrive and pages with an increasing offset', async () => {
    const m = makeModel({ pageSize: 4 });
    m.ensureLoaded();
    expect(m.status).toBe('loading');
    expect(calls[0]!.request).toMatchObject({ offset: 0, limit: 4, rawWhere: null, sort: [] });

    calls[0]!.emit({ type: 'columns', columns: COLUMNS });
    calls[0]!.emit({ type: 'rows', rows: rows(0, 2) });
    expect(m.rows).toHaveLength(2); // first rows visible before the page ends
    expect(m.columns).toEqual(COLUMNS);
    calls[0]!.emit({ type: 'rows', rows: rows(2, 2) });
    calls[0]!.resolve();
    await flush();
    expect(m.status).toBe('ready');
    expect(m.exhausted).toBe(false);

    m.loadMore();
    expect(calls[1]!.request).toMatchObject({ offset: 4, limit: 4 });
    feed(calls[1]!, 4, 1);
    calls[1]!.resolve();
    await flush();
    expect(m.rows).toHaveLength(5);
    expect(m.exhausted).toBe(true); // short page = end of data

    m.loadMore();
    expect(calls).toHaveLength(2);
  });

  it('ignores loadMore while loading and ensureLoaded twice', () => {
    const m = makeModel({ pageSize: 4 });
    m.ensureLoaded();
    m.ensureLoaded();
    m.loadMore();
    expect(calls).toHaveLength(1);
  });

  it('stops at the row cap and flags it', async () => {
    const m = makeModel({ pageSize: 3, maxRows: 5 });
    m.ensureLoaded();
    feed(calls[0]!, 0, 3);
    calls[0]!.resolve();
    await flush();
    m.loadMore();
    expect(calls[1]!.request).toMatchObject({ offset: 3, limit: 2 }); // trimmed to the cap
    feed(calls[1]!, 3, 2);
    calls[1]!.resolve();
    await flush();
    expect(m.rows).toHaveLength(5);
    expect(m.capped).toBe(true);
    expect(m.exhausted).toBe(true);
    m.loadMore();
    expect(calls).toHaveLength(2);
  });
});

describe('GridModel reset on sort / filter', () => {
  it('sort change cancels the running fetch, clears rows and ignores its late events', async () => {
    const m = makeModel({ pageSize: 10 });
    m.ensureLoaded();
    const first = calls[0]!;
    first.emit({ type: 'columns', columns: COLUMNS });
    first.emit({ type: 'rows', rows: rows(0, 3) });

    m.toggleSort('id', false);
    expect(ipc.queryCancel).toHaveBeenCalledWith('s1', first.request.queryId);
    expect(m.rows).toHaveLength(0);
    expect(calls).toHaveLength(2);
    expect(calls[1]!.request).toMatchObject({
      offset: 0,
      sort: [{ column: 'id', descending: false }],
    });
    expect(calls[1]!.request.queryId).not.toBe(first.request.queryId);

    // Late events and completion of the old fetch must not touch the new state.
    first.emit({ type: 'rows', rows: rows(100, 5) });
    first.resolve();
    await flush();
    expect(m.rows).toHaveLength(0);
    expect(m.status).toBe('loading');

    feed(calls[1]!, 0, 2);
    calls[1]!.resolve();
    await flush();
    expect(m.rows).toEqual(rows(0, 2));
    expect(m.status).toBe('ready');
  });

  it('ignores a late error from a superseded fetch', async () => {
    const m = makeModel();
    m.ensureLoaded();
    const first = calls[0]!;
    m.toggleSort('id', false);
    first.reject({ kind: 'cancelled', message: 'operation cancelled' });
    await flush();
    expect(m.status).toBe('loading');
    expect(m.error).toBeNull();
  });

  it('applyFilter sends filters / rawWhere, resets rows and the count', async () => {
    const m = makeModel();
    vi.mocked(ipc.tableCount).mockResolvedValue(42);
    m.ensureLoaded();
    feed(calls[0]!, 0, 3);
    calls[0]!.resolve();
    await flush();
    m.count();
    await flush();
    expect(m.total).toBe(42);

    m.applyFilter({
      ...emptyDraft(),
      rows: [{ id: 'a', enabled: true, column: 'id', op: 'gt', value: '1' }],
    });
    expect(m.rows).toHaveLength(0);
    expect(m.total).toBeNull();
    expect(calls[1]!.request.filters).toEqual([{ column: 'id', op: 'gt', values: ['1'] }]);

    m.applyFilter({ ...emptyDraft(), mode: 'raw', raw: 'id < 3' });
    expect(calls[2]!.request).toMatchObject({ filters: [], rawWhere: 'id < 3', offset: 0 });

    m.clearFilter();
    expect(calls[3]!.request).toMatchObject({ filters: [], rawWhere: null });
  });

  it('count uses the applied filter and discards a stale result', async () => {
    const m = makeModel();
    let resolveCount: (n: number) => void = () => undefined;
    vi.mocked(ipc.tableCount).mockImplementation(
      () => new Promise<number>((r) => (resolveCount = r)),
    );
    m.applyFilter({ ...emptyDraft(), mode: 'raw', raw: 'x = 1' });
    m.count();
    expect(ipc.tableCount).toHaveBeenCalledWith('s1', {
      table: m.table,
      filters: [],
      rawWhere: 'x = 1',
      queryId: expect.any(String),
    });
    expect(m.counting).toBe(true);
    m.clearFilter(); // invalidates the in-flight count
    resolveCount(7);
    await flush();
    expect(m.total).toBeNull();
    expect(m.counting).toBe(false);
  });
});

describe('GridModel errors and stop', () => {
  it('surfaces a server error and retries from the same offset', async () => {
    const m = makeModel({ pageSize: 3 });
    m.ensureLoaded();
    feed(calls[0]!, 0, 3);
    calls[0]!.resolve();
    await flush();
    m.loadMore();
    calls[1]!.reject({ kind: 'query', message: 'syntax error at or near "foo"' });
    await flush();
    expect(m.status).toBe('error');
    expect(m.error?.message).toContain('syntax error');
    expect(m.rows).toHaveLength(3);

    m.retry();
    expect(calls[2]!.request.offset).toBe(3);
    expect(m.error).toBeNull();
  });

  it('retry after a first-page error reloads from scratch', async () => {
    const m = makeModel();
    m.ensureLoaded();
    calls[0]!.reject({ kind: 'query', message: 'bad where' });
    await flush();
    m.retry();
    expect(calls[1]!.request.offset).toBe(0);
  });

  it('Stop cancels the fetch and keeps the rows received so far', async () => {
    const m = makeModel({ pageSize: 10 });
    m.ensureLoaded();
    feed(calls[0]!, 0, 4);
    m.stop();
    expect(ipc.queryCancel).toHaveBeenCalledWith('s1', calls[0]!.request.queryId);
    expect(m.status).toBe('stopped'); // immediately, without waiting for the backend
    calls[0]!.emit({ type: 'rows', rows: rows(4, 3) }); // late rows of the stopped fetch
    calls[0]!.reject({ kind: 'cancelled', message: 'operation cancelled' });
    await flush();
    expect(m.status).toBe('stopped');
    expect(m.rows).toHaveLength(4);
    m.loadMore(); // no automatic resume
    expect(calls).toHaveLength(1);
    m.retry();
    expect(calls[1]!.request.offset).toBe(4);
  });

  it('dispose cancels and silences listeners', () => {
    const m = makeModel();
    const listener = vi.fn();
    m.subscribe(listener);
    m.ensureLoaded();
    listener.mockClear();
    m.dispose();
    expect(ipc.queryCancel).toHaveBeenCalled();
    calls[0]!.emit({ type: 'rows', rows: rows(0, 1) });
    expect(listener).not.toHaveBeenCalled();
  });

  it('coalesces row batches into one notification per frame', () => {
    const pending: (() => void)[] = [];
    const m = new GridModel(
      { sessionId: 's1', table: { segments: [] } },
      { request: (cb) => (pending.push(cb), pending.length), cancel: () => undefined },
    );
    const listener = vi.fn();
    m.subscribe(listener);
    m.ensureLoaded(); // status -> loading notifies once
    listener.mockClear();
    for (let i = 0; i < 5; i++) calls[0]!.emit({ type: 'rows', rows: rows(i, 1) });
    expect(listener).not.toHaveBeenCalled();
    expect(pending).toHaveLength(1);
    pending[0]!();
    expect(listener).toHaveBeenCalledTimes(1);
    expect(m.rows).toHaveLength(5);
  });
});

describe('GridModel end detection, budgets and stop', () => {
  const end = (rowCount: number): QueryEvent => ({
    type: 'statementEnd',
    end: { outcome: { kind: 'rows', rowCount, truncated: false }, elapsedMs: 1 },
  });

  it('decides the end from statementEnd rowCount, not from rows counted at resolve time', async () => {
    const m = makeModel({ pageSize: 4 });
    m.ensureLoaded();
    calls[0]!.emit({ type: 'columns', columns: COLUMNS });
    calls[0]!.emit({ type: 'rows', rows: rows(0, 2) }); // rest of the page "still in flight"
    calls[0]!.emit(end(4));
    calls[0]!.resolve();
    await flush();
    expect(m.exhausted).toBe(false); // a full page of 4 was reported
    m.loadMore();
    expect(calls[1]!.request.offset).toBe(2); // offset follows the rows actually held
    calls[1]!.emit(end(1));
    calls[1]!.resolve();
    await flush();
    expect(m.exhausted).toBe(true);
  });

  it('grows the page size with each page', async () => {
    const m = new GridModel({ sessionId: 's1', table: { segments: [] } }, sync);
    m.ensureLoaded();
    expect(calls[0]!.request.limit).toBe(1000);
    calls[0]!.emit({ type: 'columns', columns: COLUMNS });
    calls[0]!.emit({ type: 'rows', rows: rows(0, 1000) });
    calls[0]!.resolve();
    await flush();
    m.loadMore();
    expect(calls[1]!.request.limit).toBe(5000);
    calls[1]!.emit({ type: 'rows', rows: rows(1000, 5000) });
    calls[1]!.resolve();
    await flush();
    m.loadMore();
    expect(calls[2]!.request.limit).toBe(20000);
    calls[2]!.emit({ type: 'rows', rows: rows(6000, 20000) });
    calls[2]!.resolve();
    await flush();
    m.loadMore();
    expect(calls[3]!.request.limit).toBe(20000);
    expect(calls[3]!.request.batchSize).toBeGreaterThanOrEqual(500);
    expect(calls[3]!.request.batchSize).toBeLessThanOrEqual(1000);
  });

  it('stops on the cell budget (rows x columns) and says why', async () => {
    const m = new GridModel(
      { sessionId: 's1', table: { segments: [] }, pageSize: 4, maxCells: 10, maxRows: 100 },
      sync,
    );
    const two: ColumnMeta[] = [COLUMNS[0]!, { name: 'b', typeName: 'int', valueKind: 'int' }];
    m.ensureLoaded();
    calls[0]!.emit({ type: 'columns', columns: two });
    calls[0]!.emit({ type: 'rows', rows: rows(0, 4).map((r) => [...r, ...r]) });
    calls[0]!.resolve();
    await flush();
    m.loadMore();
    expect(calls[1]!.request.limit).toBe(1); // 10 cells / 2 columns = 5 rows max
    calls[1]!.emit({ type: 'rows', rows: rows(4, 1).map((r) => [...r, ...r]) });
    calls[1]!.resolve();
    await flush();
    expect(m.capped).toBe(true);
    expect(m.capReason).toBe('cells');
    expect(m.rows).toHaveLength(5);
    m.loadMore();
    expect(calls).toHaveLength(2);
  });

  it('reports the row cap reason', async () => {
    const m = makeModel({ pageSize: 3, maxRows: 3 });
    m.ensureLoaded();
    feed(calls[0]!, 0, 3);
    calls[0]!.resolve();
    await flush();
    expect(m.capReason).toBe('rows');
  });

  it('calls onReset on every reload', () => {
    const onReset = vi.fn();
    const m = new GridModel({ sessionId: 's1', table: { segments: [] }, onReset }, sync);
    m.ensureLoaded();
    m.toggleSort('id', false);
    m.applyFilter(emptyDraft());
    expect(onReset).toHaveBeenCalledTimes(3);
  });

  it('retries the cancel when the backend does not know the query yet', async () => {
    vi.mocked(ipc.queryCancel)
      .mockRejectedValueOnce({ kind: 'notFound', message: 'query' })
      .mockResolvedValue(undefined);
    const m = makeModel();
    m.ensureLoaded();
    m.stop();
    expect(ipc.queryCancel).toHaveBeenCalledTimes(1);
    await new Promise((r) => setTimeout(r, CANCEL_RETRY_DELAY_MS + 100));
    expect(ipc.queryCancel).toHaveBeenCalledTimes(2);
    // The late result of the cancelled fetch is ignored: the state stays "stopped".
    calls[0]!.emit({ type: 'rows', rows: rows(0, 3) });
    calls[0]!.resolve();
    await flush();
    expect(m.status).toBe('stopped');
    expect(m.rows).toHaveLength(0);
  });
});

describe('GridModel cancellable count', () => {
  it('sends a queryId and cancels with it; cancel returns to "not counted" without an error', async () => {
    let rejectCount: (e: IpcError) => void = () => undefined;
    vi.mocked(ipc.tableCount).mockImplementation(
      () => new Promise<number>((_, rej) => (rejectCount = rej)),
    );
    const m = makeModel();
    m.count();
    const request = vi.mocked(ipc.tableCount).mock.calls[0]![1];
    expect(request.queryId).toMatch(/[0-9a-f-]{36}/);
    expect(m.counting).toBe(true);

    m.cancelCount();
    expect(ipc.queryCancel).toHaveBeenCalledWith('s1', request.queryId);
    expect(m.counting).toBe(false);
    expect(m.total).toBeNull();
    rejectCount({ kind: 'cancelled', message: 'operation cancelled' }); // backend confirms later
    await flush();
    expect(m.countError).toBeNull();
    expect(m.total).toBeNull();
  });

  it('a cancelled error from the backend is not shown; other errors are kept compactly', async () => {
    vi.mocked(ipc.tableCount).mockRejectedValueOnce({ kind: 'cancelled', message: 'x' });
    const m = makeModel();
    m.count();
    await flush();
    expect(m.countError).toBeNull();
    expect(m.counting).toBe(false);
    vi.mocked(ipc.tableCount).mockRejectedValueOnce({ kind: 'query', message: 'boom' });
    m.count();
    await flush();
    expect(m.countError?.message).toBe('boom');
    expect(m.status).toBe('idle'); // the grid itself is untouched
  });

  it('sort, refresh and dispose cancel a running count and drop its late result', async () => {
    let resolveCount: (n: number) => void = () => undefined;
    vi.mocked(ipc.tableCount).mockImplementation(
      () => new Promise<number>((r) => (resolveCount = r)),
    );
    const m = makeModel();
    m.count();
    const id = vi.mocked(ipc.tableCount).mock.calls[0]![1].queryId;
    m.toggleSort('id', false);
    expect(ipc.queryCancel).toHaveBeenCalledWith('s1', id);
    expect(m.counting).toBe(false);
    resolveCount(99);
    await flush();
    expect(m.total).toBeNull();

    m.count();
    const id2 = vi.mocked(ipc.tableCount).mock.calls[1]![1].queryId;
    m.dispose();
    expect(ipc.queryCancel).toHaveBeenCalledWith('s1', id2);
  });
});
