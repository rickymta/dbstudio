import { Channel, invoke } from '@tauri-apps/api/core';
import { mock, mockAppInfo, mockDrivers, mockLogDir } from './mock';
import type {
  AppInfo,
  AppSettings,
  ConnectionGroup,
  ConnectionProfile,
  ConnectionsSnapshot,
  DriverDescriptor,
  ExecSummary,
  IpcError,
  ObjectNode,
  ObjectPath,
  ObjectSearchRequest,
  QueryEvent,
  SecretChange,
  ServerInfo,
  SessionInfo,
  TableCountRequest,
  TableDataRequest,
  TableDetails,
  Uuid,
} from './types';

/** True when running inside the Tauri webview (as opposed to a plain browser during dev). */
export function isTauri(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

const ERROR_KINDS: ReadonlySet<string> = new Set([
  'connection',
  'authentication',
  'query',
  'unsupported',
  'cancelled',
  'timeout',
  'tls',
  'notFound',
  'storage',
  'io',
  'other',
]);

/** Normalize anything thrown by `invoke` (or the mock) into an [`IpcError`]. */
export function toIpcError(e: unknown): IpcError {
  if (typeof e === 'object' && e !== null && 'kind' in e && 'message' in e) {
    const o = e as Record<string, unknown>;
    if (typeof o.kind === 'string' && typeof o.message === 'string') {
      const err: IpcError = {
        kind: (ERROR_KINDS.has(o.kind) ? o.kind : 'other') as IpcError['kind'],
        message: o.message,
      };
      if (typeof o.position === 'number') err.position = o.position;
      if (typeof o.capability === 'string') err.capability = o.capability;
      return err;
    }
  }
  if (typeof e === 'string') return { kind: 'other', message: e };
  if (e instanceof Error) return { kind: 'other', message: e.message };
  return { kind: 'other', message: String(e) };
}

export type IpcTransport = 'tauri' | 'bridge' | 'mock';

// The bridge is a development aid: both variables are only read when `import.meta.env.DEV` is
// true, so production builds drop the whole branch (and never contain a bridge token).
const bridgeUrl = (): string | undefined => {
  if (!import.meta.env.DEV) return undefined;
  const url = import.meta.env.VITE_IPC_BRIDGE_URL as string | undefined;
  return url ? url.replace(/\/+$/, '') : undefined;
};

/** How backend calls are carried: the Tauri runtime, the dev HTTP bridge, or the in-memory mock. */
export function ipcTransport(): IpcTransport {
  if (isTauri()) return 'tauri';
  return bridgeUrl() ? 'bridge' : 'mock';
}

function bridgeRequest(command: string, body: unknown, signal?: AbortSignal): Promise<Response> {
  const headers: Record<string, string> = { 'content-type': 'application/json' };
  const token = import.meta.env.DEV
    ? (import.meta.env.VITE_IPC_BRIDGE_TOKEN as string | undefined)
    : undefined;
  if (token) headers['x-dbstudio-bridge-token'] = token;
  return fetch(`${bridgeUrl()}/ipc/${command}`, {
    method: 'POST',
    headers,
    body: JSON.stringify(body ?? {}),
    signal,
  }).catch((e: unknown) => {
    throw {
      kind: 'connection',
      message: `IPC bridge unreachable: ${e instanceof Error ? e.message : String(e)}`,
    } satisfies IpcError;
  });
}

async function bridgeFailure(res: Response): Promise<IpcError> {
  const text = await res.text().catch(() => '');
  try {
    return toIpcError(JSON.parse(text));
  } catch {
    return { kind: 'other', message: text || `IPC bridge error: HTTP ${res.status}` };
  }
}

async function bridgeCall<T>(command: string, body: unknown): Promise<T> {
  const res = await bridgeRequest(command, body);
  if (res.status !== 200) throw await bridgeFailure(res);
  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

/** In-flight bridge fetches by query id, so `queryCancel` can drop the local stream too. */
const bridgeFetches = new Map<string, AbortController>();

const CANCELLED: IpcError = { kind: 'cancelled', message: 'query cancelled' };

/** POST `table_fetch` and read the NDJSON event stream until its `result`/`error` line. */
async function bridgeStream(
  body: { sessionId: Uuid; request: TableDataRequest },
  handler: (e: QueryEvent) => void,
): Promise<ExecSummary> {
  const controller = new AbortController();
  const queryId = body.request.queryId;
  if (queryId) bridgeFetches.set(queryId, controller);
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  try {
    const res = await bridgeRequest('table_fetch', body, controller.signal);
    if (res.status !== 200) throw await bridgeFailure(res);
    if (!res.body) throw { kind: 'connection', message: 'IPC bridge returned no stream' };
    reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    let summary: ExecSummary | null = null;

    const handleLine = (line: string) => {
      if (!line.trim()) return;
      const msg = JSON.parse(line) as { type: string; summary?: ExecSummary; error?: unknown };
      if (msg.type === 'result') summary = msg.summary ?? { statements: 0, elapsedMs: 0 };
      else if (msg.type === 'error') throw toIpcError(msg.error);
      else handler(msg as unknown as QueryEvent);
    };

    for (;;) {
      let chunk: ReadableStreamReadResult<Uint8Array>;
      try {
        chunk = await reader.read();
      } catch (e) {
        if (controller.signal.aborted) throw CANCELLED;
        throw { kind: 'connection', message: `IPC bridge stream failed: ${String(e)}` };
      }
      buffer += decoder.decode(chunk.value, { stream: !chunk.done });
      let newline = buffer.indexOf('\n');
      while (newline >= 0) {
        handleLine(buffer.slice(0, newline));
        buffer = buffer.slice(newline + 1);
        newline = buffer.indexOf('\n');
      }
      if (chunk.done) break;
    }
    handleLine(buffer);
    if (!summary) throw { kind: 'connection', message: 'IPC bridge stream ended without a result' };
    return summary;
  } catch (e) {
    if (controller.signal.aborted) throw CANCELLED;
    throw e;
  } finally {
    // Always release the connection, whether the stream finished, failed or was cancelled.
    if (queryId && bridgeFetches.get(queryId) === controller) bridgeFetches.delete(queryId);
    await reader?.cancel().catch(() => undefined);
    controller.abort();
  }
}

/**
 * Call a backend command: through Tauri, the dev HTTP bridge (`VITE_IPC_BRIDGE_URL`), or the mock.
 * Errors are normalized to [`IpcError`]. All backend access must go through this module.
 */
async function call<T>(
  command: string,
  mockFn: () => T | Promise<T>,
  args?: Record<string, unknown> | (() => Record<string, unknown>),
): Promise<T> {
  try {
    const transport = ipcTransport();
    if (transport === 'mock') return await mockFn();
    const resolved = typeof args === 'function' ? args() : args;
    if (transport === 'bridge') return await bridgeCall<T>(command, resolved);
    return await invoke<T>(command, resolved);
  } catch (e) {
    throw toIpcError(e);
  }
}

/** How long to wait for `statementEnd` after the command resolved before giving up. */
export const STATEMENT_END_TIMEOUT_MS = 5000;

function streamFetch(
  onEvent: (e: QueryEvent) => void,
  start: (handler: (e: QueryEvent) => void) => Promise<ExecSummary>,
): Promise<ExecSummary> {
  return new Promise<ExecSummary>((resolve, reject) => {
    // Per-call flag: once settled, events that still arrive for this fetch are dropped.
    let closed = false;
    let ended = false;
    let summary: ExecSummary | null = null;
    let timer: ReturnType<typeof setTimeout> | undefined;

    const finish = () => {
      if (closed || !summary || !ended) return;
      closed = true;
      clearTimeout(timer);
      resolve(summary);
    };

    const handler = (e: QueryEvent) => {
      if (closed) return;
      onEvent(e);
      if (e.type === 'statementEnd') {
        ended = true;
        finish();
      }
    };

    let pending: Promise<ExecSummary>;
    try {
      pending = start(handler);
    } catch (e) {
      closed = true;
      reject(toIpcError(e));
      return;
    }
    pending.then(
      (s) => {
        if (closed) return;
        summary = s;
        if (ended) return finish();
        timer = setTimeout(() => {
          if (closed) return;
          closed = true;
          console.warn('table_fetch resolved but no statementEnd arrived; continuing');
          resolve(s);
        }, STATEMENT_END_TIMEOUT_MS);
      },
      (e) => {
        if (closed) return;
        closed = true;
        clearTimeout(timer);
        reject(toIpcError(e));
      },
    );
  });
}

export const ipc = {
  appInfo: (): Promise<AppInfo> => call('app_info', () => mockAppInfo),
  listDrivers: (): Promise<DriverDescriptor[]> => call('list_drivers', () => mockDrivers),

  connectionsList: (): Promise<ConnectionsSnapshot> =>
    call('connections_list', () => mock.connectionsList()),
  connectionSave: (profile: ConnectionProfile, secret: SecretChange): Promise<ConnectionProfile> =>
    call('connection_save', () => mock.connectionSave(profile, secret), { profile, secret }),
  connectionDelete: (id: Uuid): Promise<void> =>
    call('connection_delete', () => mock.connectionDelete(id), { id }),
  connectionDuplicate: (id: Uuid): Promise<ConnectionProfile> =>
    call('connection_duplicate', () => mock.connectionDuplicate(id), { id }),
  groupSave: (group: ConnectionGroup): Promise<ConnectionGroup> =>
    call('group_save', () => mock.groupSave(group), { group }),
  groupDelete: (id: Uuid): Promise<void> =>
    call('group_delete', () => mock.groupDelete(id), { id }),
  connectionTest: (profile: ConnectionProfile, secret: SecretChange): Promise<ServerInfo> =>
    call('connection_test', () => mock.connectionTest(profile), { profile, secret }),
  connectionsExport: (path: string, ids?: Uuid[] | null): Promise<number> =>
    call('connections_export', () => mock.connectionsExport(ids ?? null), {
      path,
      ids: ids ?? null,
    }),
  connectionsImport: (path: string): Promise<ConnectionsSnapshot> =>
    call('connections_import', () => mock.connectionsImport(), { path }),

  sessionOpen: (connectionId: Uuid, password?: string | null): Promise<SessionInfo> =>
    call('session_open', () => mock.sessionOpen(connectionId, password ?? null), {
      connectionId,
      password: password ?? null,
    }),
  sessionClose: (sessionId: Uuid): Promise<void> =>
    call('session_close', () => mock.sessionClose(sessionId), { sessionId }),
  objectChildren: (sessionId: Uuid, path: ObjectPath): Promise<ObjectNode[]> =>
    call('object_children', () => mock.objectChildren(sessionId, path), { sessionId, path }),
  objectSearch: (sessionId: Uuid, request: ObjectSearchRequest): Promise<ObjectNode[]> =>
    call('object_search', () => mock.objectSearch(sessionId, request), { sessionId, request }),
  objectDdl: (sessionId: Uuid, path: ObjectPath): Promise<string> =>
    call('object_ddl', () => mock.objectDdl(sessionId, path), { sessionId, path }),
  tableDetails: (sessionId: Uuid, path: ObjectPath): Promise<TableDetails> =>
    call('table_details', () => mock.tableDetails(sessionId, path), { sessionId, path }),
  tableCount: (sessionId: Uuid, request: TableCountRequest): Promise<number> =>
    call('table_count', () => mock.tableCount(sessionId, request), { sessionId, request }),
  queryCancel: async (sessionId: Uuid, queryId: Uuid): Promise<void> => {
    try {
      await call<void>('query_cancel', () => mock.queryCancel(sessionId, queryId), {
        sessionId,
        queryId,
      });
    } finally {
      // Over the dev bridge, also stop reading the local stream of that fetch.
      bridgeFetches.get(queryId)?.abort();
    }
  },

  settingsGet: (): Promise<AppSettings> => call('settings_get', () => mock.settingsGet()),
  settingsSet: (settings: AppSettings): Promise<AppSettings> =>
    call('settings_set', () => mock.settingsSet(settings), { settings }),
  logDir: (): Promise<string> => call('log_dir', () => mockLogDir),
  openLogDir: (): Promise<void> => call('open_log_dir', () => undefined),
  /** Save bytes (base64) through a native save dialog; resolves to the path, `null` if cancelled. */
  saveCellValue: (suggestedName: string, dataBase64: string): Promise<string | null> =>
    call('save_cell_value', () => mock.saveCellValue(suggestedName, dataBase64), {
      suggestedName,
      dataBase64,
    }),

  /**
   * Stream a page of table rows; `onEvent` receives columns, row batches and the end marker.
   * Resolves only after BOTH the command returned and `statementEnd` was received: Tauri delivers
   * large Channel payloads on a separate async round trip, so row events may arrive after the
   * `invoke` promise settled. The backend always sends `statementEnd` after the last `rows`, and a
   * Channel preserves message order, so `statementEnd` means every row was delivered.
   */
  tableFetch: (
    sessionId: Uuid,
    request: TableDataRequest,
    onEvent: (e: QueryEvent) => void,
  ): Promise<ExecSummary> =>
    streamFetch(onEvent, (handler) => {
      const transport = ipcTransport();
      if (transport === 'mock') return mock.tableFetch(sessionId, request, handler);
      if (transport === 'bridge') return bridgeStream({ sessionId, request }, handler);
      const channel = new Channel<QueryEvent>();
      channel.onmessage = handler;
      return invoke<ExecSummary>('table_fetch', { sessionId, request, onEvent: channel });
    }),
};
