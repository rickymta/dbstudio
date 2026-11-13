import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ipc, ipcTransport } from './ipc';
import { ROOT_PATH } from './objectPath';
import type { QueryEvent, TableDataRequest } from './types';

const URL_BASE = 'http://127.0.0.1:9999';

function jsonResponse(status: number, body: unknown): Response {
  return new Response(body === undefined ? '' : JSON.stringify(body), { status });
}

function streamResponse(chunks: string[]): Response {
  const encoder = new TextEncoder();
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const c of chunks) controller.enqueue(encoder.encode(c));
      controller.close();
    },
  });
  return new Response(body, { status: 200 });
}

/** Stream raw bytes cut at arbitrary offsets (may split multi-byte UTF-8 characters). */
function byteStreamResponse(text: string, cuts: number[]): Response {
  const bytes = new TextEncoder().encode(text);
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      let from = 0;
      for (const cut of [...cuts, bytes.length]) {
        controller.enqueue(bytes.slice(from, cut));
        from = cut;
      }
      controller.close();
    },
  });
  return new Response(body, { status: 200 });
}

/** A response whose stream never ends; records cancel and follows the fetch abort signal. */
function endlessResponse(signal: AbortSignal | undefined, onCancel: () => void): Response {
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      signal?.addEventListener('abort', () =>
        controller.error(new DOMException('aborted', 'AbortError')),
      );
    },
    cancel: onCancel,
  });
  return new Response(body, { status: 200 });
}

const request: TableDataRequest = {
  queryId: 'q',
  table: ROOT_PATH,
  filters: [],
  sort: [],
  offset: 0,
  limit: 10,
};

beforeEach(() => {
  vi.stubEnv('VITE_IPC_BRIDGE_URL', URL_BASE + '/');
  vi.stubEnv('VITE_IPC_BRIDGE_TOKEN', 'tok');
});
afterEach(() => {
  vi.unstubAllEnvs();
  vi.unstubAllGlobals();
});

describe('ipc dev bridge', () => {
  it('reports its transport', () => {
    expect(ipcTransport()).toBe('bridge');
    vi.stubEnv('VITE_IPC_BRIDGE_URL', '');
    expect(ipcTransport()).toBe('mock');
  });

  it('posts commands with the token and returns the JSON result', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(200, { name: 'real' }));
    vi.stubGlobal('fetch', fetchMock);
    const info = await ipc.appInfo();
    expect(info).toEqual({ name: 'real' });
    const [url, init] = fetchMock.mock.calls[0]!;
    expect(url).toBe(`${URL_BASE}/ipc/app_info`);
    expect(init.method).toBe('POST');
    expect(init.headers).toEqual({
      'content-type': 'application/json',
      'x-dbstudio-bridge-token': 'tok',
    });
    expect(init.body).toBe('{}');

    fetchMock.mockResolvedValueOnce(jsonResponse(200, []));
    await ipc.objectSearch('s1', { query: 'u', scope: ROOT_PATH, limit: 5 });
    const [url2, init2] = fetchMock.mock.calls[1]!;
    expect(url2).toBe(`${URL_BASE}/ipc/object_search`);
    expect(JSON.parse(init2.body)).toEqual({
      sessionId: 's1',
      request: { query: 'u', scope: ROOT_PATH, limit: 5 },
    });
  });

  it('accepts an empty body for void commands', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(200, undefined)));
    await expect(ipc.sessionClose('s')).resolves.toBeUndefined();
  });

  it('turns non-200 responses into IpcError', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(jsonResponse(400, { kind: 'authentication', message: 'bad' })),
    );
    await expect(ipc.connectionsList()).rejects.toEqual({
      kind: 'authentication',
      message: 'bad',
    });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response('oops', { status: 502 })));
    await expect(ipc.connectionsList()).rejects.toMatchObject({ kind: 'other', message: 'oops' });
  });

  it('maps network failures to a connection error', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new TypeError('Failed to fetch')));
    await expect(ipc.connectionsList()).rejects.toMatchObject({ kind: 'connection' });
  });

  it('streams table_fetch NDJSON even when lines are split across chunks', async () => {
    const lines = [
      JSON.stringify({ type: 'columns', columns: [] }),
      JSON.stringify({ type: 'rows', rows: [[{ t: 'int', v: 1 }]] }),
      JSON.stringify({
        type: 'statementEnd',
        end: { outcome: { kind: 'rows', rowCount: 1, truncated: false }, elapsedMs: 1 },
      }),
      JSON.stringify({ type: 'result', summary: { statements: 1, elapsedMs: 2 } }),
    ].join('\n');
    // Cut in the middle of a line and of a multi-byte-free JSON token.
    const chunks = [
      lines.slice(0, 20),
      lines.slice(20, 75),
      lines.slice(75, 140),
      lines.slice(140) + '\n',
    ];
    const fetchMock = vi.fn().mockResolvedValue(streamResponse(chunks));
    vi.stubGlobal('fetch', fetchMock);
    const events: QueryEvent[] = [];
    const summary = await ipc.tableFetch('s', request, (e) => events.push(e));
    expect(summary).toEqual({ statements: 1, elapsedMs: 2 });
    expect(events.map((e) => e.type)).toEqual(['columns', 'rows', 'statementEnd']);
    expect(fetchMock.mock.calls[0]![0]).toBe(`${URL_BASE}/ipc/table_fetch`);
    expect(JSON.parse(fetchMock.mock.calls[0]![1].body)).toEqual({ sessionId: 's', request });
  });

  it('handles a final result line without a trailing newline', async () => {
    const body = [
      JSON.stringify({
        type: 'statementEnd',
        end: { outcome: { kind: 'done' }, elapsedMs: 1 },
      }),
      JSON.stringify({ type: 'result', summary: { statements: 1, elapsedMs: 3 } }),
    ].join('\n');
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(streamResponse([body])));
    await expect(ipc.tableFetch('s', request, () => {})).resolves.toEqual({
      statements: 1,
      elapsedMs: 3,
    });
  });

  it('decodes multi-byte UTF-8 split at every possible byte offset', async () => {
    const text = 'Đơn hàng 😀 ñ';
    const lines =
      [
        JSON.stringify({ type: 'message', message: text }),
        JSON.stringify({
          type: 'statementEnd',
          end: { outcome: { kind: 'done' }, elapsedMs: 1 },
        }),
        JSON.stringify({ type: 'result', summary: { statements: 1, elapsedMs: 1 } }),
      ].join('\n') + '\n';
    const length = new TextEncoder().encode(lines).length;
    for (let cut = 1; cut < length; cut += 1) {
      vi.stubGlobal('fetch', vi.fn().mockResolvedValue(byteStreamResponse(lines, [cut, cut + 2])));
      const events: QueryEvent[] = [];
      await ipc.tableFetch('s', request, (e) => events.push(e));
      expect(events[0]).toEqual({ type: 'message', message: text });
    }
  });

  it('rejects on an error line and on a stream that ends early', async () => {
    const err = JSON.stringify({ type: 'error', error: { kind: 'query', message: 'boom' } });
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(streamResponse([err.slice(0, 10), err.slice(10) + '\n'])),
    );
    await expect(ipc.tableFetch('s', request, () => {})).rejects.toEqual({
      kind: 'query',
      message: 'boom',
    });
    vi.stubGlobal(
      'fetch',
      vi
        .fn()
        .mockResolvedValue(
          streamResponse([JSON.stringify({ type: 'columns', columns: [] }) + '\n']),
        ),
    );
    await expect(ipc.tableFetch('s', request, () => {})).rejects.toMatchObject({
      kind: 'connection',
    });
  });
});

describe('bridge cancellation and production gating', () => {
  it('queryCancel aborts the local stream and cancels the reader', async () => {
    let signal: AbortSignal | undefined;
    const fetchMock = vi.fn().mockImplementation((url: string, init: RequestInit) => {
      if (url.endsWith('/ipc/query_cancel')) return Promise.resolve(jsonResponse(200, undefined));
      signal = init.signal ?? undefined;
      return Promise.resolve(endlessResponse(init.signal ?? undefined, () => {}));
    });
    vi.stubGlobal('fetch', fetchMock);
    const pending = ipc.tableFetch('s', request, () => {});
    const outcome = pending.then(
      () => 'done',
      (e: { kind: string }) => e.kind,
    );
    await vi.waitFor(() => expect(signal).toBeDefined());
    await ipc.queryCancel('s', request.queryId);
    expect(await outcome).toBe('cancelled');
    expect(signal!.aborted).toBe(true);
    expect(fetchMock.mock.calls.some(([u]) => String(u).endsWith('/ipc/query_cancel'))).toBe(true);
  });

  it('releases the stream when a line cannot be parsed', async () => {
    let cancelled = false;
    let signal: AbortSignal | undefined;
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(new TextEncoder().encode('not json\n'));
      },
      cancel: () => {
        cancelled = true;
      },
    });
    vi.stubGlobal(
      'fetch',
      vi.fn().mockImplementation((_u: string, init: RequestInit) => {
        signal = init.signal ?? undefined;
        return Promise.resolve(new Response(body, { status: 200 }));
      }),
    );
    await expect(ipc.tableFetch('s', request, () => {})).rejects.toMatchObject({ kind: 'other' });
    expect(cancelled).toBe(true);
    expect(signal!.aborted).toBe(true);
  });

  it('never uses the bridge outside development builds', () => {
    expect(ipcTransport()).toBe('bridge');
    vi.stubEnv('DEV', false);
    expect(ipcTransport()).toBe('mock');
  });
});

describe('new mock commands', () => {
  beforeEach(() => vi.unstubAllEnvs());

  it('keeps settings in memory and exposes log dir and save path', async () => {
    const initial = await ipc.settingsGet();
    expect(initial).toEqual({
      logLevel: 'info',
      gridMaxRows: 1_000_000,
      gridMaxCells: 20_000_000,
      timestampDisplay: 'utc',
    });
    const saved = await ipc.settingsSet({ ...initial, logLevel: 'debug' });
    expect(saved.logLevel).toBe('debug');
    expect((await ipc.settingsGet()).logLevel).toBe('debug');
    await ipc.settingsSet(initial);
    expect(await ipc.logDir()).toMatch(/log/);
    await expect(ipc.openLogDir()).resolves.toBeUndefined();
    expect(await ipc.saveCellValue('a.bin', 'AAA=')).toContain('a.bin');
  });
});
