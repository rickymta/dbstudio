import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ExecSummary, QueryEvent, TableDataRequest } from './types';

// Mock the Tauri core API: `Channel` captures `onmessage`, `invoke` is controllable.
const hoisted = vi.hoisted(() => {
  class FakeChannel {
    onmessage: (m: unknown) => void = () => undefined;
  }
  return { FakeChannel, invoke: vi.fn() };
});
vi.mock('@tauri-apps/api/core', () => ({ Channel: hoisted.FakeChannel, invoke: hoisted.invoke }));

import { ipc } from './ipc';

const request: TableDataRequest = {
  queryId: 'q1',
  table: { segments: [{ kind: 'table', name: 't' }] },
  filters: [],
  sort: [],
  offset: 0,
  limit: 10,
};
const summary: ExecSummary = { statements: 1, elapsedMs: 3 };
const rowsEvent: QueryEvent = { type: 'rows', rows: [[{ t: 'int', v: 1 }]] };
const endEvent: QueryEvent = {
  type: 'statementEnd',
  end: { outcome: { kind: 'rows', rowCount: 1, truncated: false }, elapsedMs: 1 },
};

function lastChannel(): InstanceType<typeof hoisted.FakeChannel> {
  const args = hoisted.invoke.mock.calls.at(-1)![1] as {
    onEvent: InstanceType<typeof hoisted.FakeChannel>;
  };
  return args.onEvent;
}

beforeEach(() => {
  hoisted.invoke.mockReset();
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
});
afterEach(() => {
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  vi.useRealTimers();
});

const flush = () => new Promise((r) => setTimeout(r, 0));

describe('ipc.tableFetch on the Tauri path', () => {
  it('invokes table_fetch with sessionId, request and a Channel', async () => {
    hoisted.invoke.mockImplementation(async (_cmd, args) => {
      (args.onEvent as { onmessage: (e: QueryEvent) => void }).onmessage(endEvent);
      return summary;
    });
    await expect(ipc.tableFetch('s1', request, () => undefined)).resolves.toEqual(summary);
    const [command, args] = hoisted.invoke.mock.calls[0]!;
    expect(command).toBe('table_fetch');
    expect(Object.keys(args).sort()).toEqual(['onEvent', 'request', 'sessionId']);
    expect(args.sessionId).toBe('s1');
    expect(args.request).toEqual(request);
    expect(args.onEvent).toBeInstanceOf(hoisted.FakeChannel);
  });

  it('waits for statementEnd when events arrive after invoke resolved', async () => {
    hoisted.invoke.mockResolvedValue(summary);
    const seen: string[] = [];
    let settled = false;
    const p = ipc
      .tableFetch('s1', request, (e) => seen.push(e.type))
      .then((r) => {
        settled = true;
        return r;
      });
    await flush();
    expect(settled).toBe(false); // invoke is done but the stream has not ended

    const ch = lastChannel();
    ch.onmessage(rowsEvent);
    await flush();
    expect(settled).toBe(false);
    ch.onmessage(endEvent);
    await expect(p).resolves.toEqual(summary);
    expect(seen).toEqual(['rows', 'statementEnd']);
  });

  it('resolves when statementEnd came before invoke resolved', async () => {
    let resolveInvoke: (s: ExecSummary) => void = () => undefined;
    hoisted.invoke.mockReturnValue(new Promise<ExecSummary>((r) => (resolveInvoke = r)));
    const p = ipc.tableFetch('s1', request, () => undefined);
    lastChannel().onmessage(rowsEvent);
    lastChannel().onmessage(endEvent);
    resolveInvoke(summary);
    await expect(p).resolves.toEqual(summary);
  });

  it('rejects immediately on invoke error and drops late events', async () => {
    hoisted.invoke.mockRejectedValue({ kind: 'query', message: 'boom' });
    const onEvent = vi.fn();
    const p = ipc.tableFetch('s1', request, onEvent);
    await expect(p).rejects.toMatchObject({ kind: 'query', message: 'boom' });
    lastChannel().onmessage(rowsEvent);
    lastChannel().onmessage(endEvent);
    expect(onEvent).not.toHaveBeenCalled();
  });

  it('gives up waiting for statementEnd after a bounded time, with a warning', async () => {
    vi.useFakeTimers();
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    hoisted.invoke.mockResolvedValue(summary);
    const onEvent = vi.fn();
    const p = ipc.tableFetch('s1', request, onEvent);
    await vi.advanceTimersByTimeAsync(5001);
    await expect(p).resolves.toEqual(summary);
    expect(warn).toHaveBeenCalled();
    lastChannel().onmessage(rowsEvent); // too late: ignored
    expect(onEvent).not.toHaveBeenCalled();
    warn.mockRestore();
  });

  it('uses the documented command names for the other table commands', async () => {
    hoisted.invoke.mockResolvedValue(5);
    await ipc.tableCount('s1', { table: request.table, filters: [] });
    expect(hoisted.invoke).toHaveBeenCalledWith('table_count', {
      sessionId: 's1',
      request: { table: request.table, filters: [] },
    });
    await ipc.queryCancel('s1', 'q1');
    expect(hoisted.invoke).toHaveBeenCalledWith('query_cancel', { sessionId: 's1', queryId: 'q1' });
  });
});
