import { Channel, invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { mock, mockAppInfo, mockDrivers, mockLogDir, mockListen } from '../mock';
import type {
  AppInfo,
  AppSettings,
  ConnectionGroup,
  ConnectionProfile,
  ConnectionsSnapshot,
  DbKind,
  DriverDescriptor,
  DriverDiagnostics,
  ExecSummary,
  IpcError,
  ObjectNode,
  ObjectPath,
  ObjectSearchRequest,
  ParsedConnectionUri,
  QueryEvent,
  SecretChange,
  ServerInfo,
  SessionInfo,
  TableCountRequest,
  TableDataRequest,
  TableDetails,
  Uuid,
} from '../types';

/** True when running inside the Tauri webview (as opposed to a plain browser during dev). */
export function isTauri(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

export const ERROR_KINDS: ReadonlySet<string> = new Set([
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
  'invalid',
  'readOnly',
  'confirmationRequired',
  'busy',
  'permission',
  'driverUnavailable',
  'sshHostKey',
  'integrity',
  'changeFailed',
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
      if (typeof o.confirmation === 'object' && o.confirmation !== null) {
        err.confirmation = o.confirmation as IpcError['confirmation'];
      }
      if (typeof o.host === 'string') err.host = o.host;
      if (typeof o.port === 'number') err.port = o.port;
      if (typeof o.keyType === 'string') err.keyType = o.keyType;
      if (typeof o.fingerprint === 'string') err.fingerprint = o.fingerprint;
      if (typeof o.changed === 'boolean') err.changed = o.changed;
      if (typeof o.index === 'number') err.index = o.index;
      if (typeof o.applied === 'number') err.applied = o.applied;
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

function bridgeHeaders(): Record<string, string> {
  const headers: Record<string, string> = { 'content-type': 'application/json' };
  const token = import.meta.env.DEV
    ? (import.meta.env.VITE_IPC_BRIDGE_TOKEN as string | undefined)
    : undefined;
  if (token) headers['x-dbstudio-bridge-token'] = token;
  return headers;
}

function unreachable(e: unknown): IpcError {
  return {
    kind: 'connection',
    message: `IPC bridge unreachable: ${e instanceof Error ? e.message : String(e)}`,
  };
}

function bridgeRequest(command: string, body: unknown, signal?: AbortSignal): Promise<Response> {
  return fetch(`${bridgeUrl()}/ipc/${command}`, {
    method: 'POST',
    headers: bridgeHeaders(),
    body: JSON.stringify(body ?? {}),
    signal,
  }).catch((e: unknown) => {
    throw unreachable(e);
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

/**
 * POST a streaming command and read the NDJSON event stream until its `result`/`error` line.
 * The result line carries the command's return value as `result` (or `summary`, the older key
 * of `table_fetch`).
 */
async function bridgeStream<R>(
  command: string,
  body: Record<string, unknown>,
  queryId: string | undefined,
  handler: (e: QueryEvent) => void,
): Promise<R> {
  const controller = new AbortController();
  if (queryId) bridgeFetches.set(queryId, controller);
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  try {
    const res = await bridgeRequest(command, body, controller.signal);
    if (res.status !== 200) throw await bridgeFailure(res);
    if (!res.body) throw { kind: 'connection', message: 'IPC bridge returned no stream' };
    reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    let result: { value: R } | null = null;

    const handleLine = (line: string) => {
      if (!line.trim()) return;
      const msg = JSON.parse(line) as {
        type: string;
        result?: R;
        summary?: R;
        error?: unknown;
      };
      if (msg.type === 'result') {
        const value = 'result' in msg ? msg.result : msg.summary;
        result = { value: (value ?? { statements: 0, elapsedMs: 0 }) as R };
      } else if (msg.type === 'error') throw toIpcError(msg.error);
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
    if (!result) throw { kind: 'connection', message: 'IPC bridge stream ended without a result' };
    return (result as { value: R }).value;
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

/** Stop reading the local bridge stream of `queryId` (no-op for other transports). */
export function abortBridgeStream(queryId: string): void {
  bridgeFetches.get(queryId)?.abort();
}

/**
 * Call a backend command: through Tauri, the dev HTTP bridge (`VITE_IPC_BRIDGE_URL`), or the mock.
 * Errors are normalized to [`IpcError`]. All backend access must go through this module.
 */
export async function call<T>(
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

/**
 * Wait for a streaming command: resolves only after BOTH the command returned and as many
 * `statementEnd` events as it ran statements were received (`expectedEnds`): Tauri delivers large
 * Channel payloads on a separate async round trip, so row events may arrive after the `invoke`
 * promise settled. A Channel preserves message order, so the last `statementEnd` means every
 * row was delivered.
 */
function streamFetch<R>(
  command: string,
  onEvent: (e: QueryEvent) => void,
  expectedEnds: (result: R) => number,
  start: (handler: (e: QueryEvent) => void) => Promise<R>,
): Promise<R> {
  return new Promise<R>((resolve, reject) => {
    // Per-call flag: once settled, events that still arrive for this fetch are dropped.
    let closed = false;
    let ends = 0;
    let settled: { value: R; expected: number } | null = null;
    let timer: ReturnType<typeof setTimeout> | undefined;

    const finish = () => {
      if (closed || !settled || ends < settled.expected) return;
      closed = true;
      clearTimeout(timer);
      resolve(settled.value);
    };

    const handler = (e: QueryEvent) => {
      if (closed) return;
      onEvent(e);
      if (e.type === 'statementEnd') {
        ends++;
        finish();
      }
    };

    let pending: Promise<R>;
    try {
      pending = start(handler);
    } catch (e) {
      closed = true;
      reject(toIpcError(e));
      return;
    }
    pending.then(
      (value) => {
        if (closed) return;
        settled = { value, expected: expectedEnds(value) };
        if (ends >= settled.expected) return finish();
        timer = setTimeout(() => {
          if (closed) return;
          closed = true;
          console.warn(`${command} resolved but statementEnd did not arrive; continuing`);
          resolve(value);
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

export interface StreamOptions<R> {
  command: string;
  /** Command arguments without the channel. */
  args: Record<string, unknown>;
  onEvent: (e: QueryEvent) => void;
  /** Query id, so `queryCancel`/`leaseCancel` can drop the local bridge stream. */
  queryId?: string;
  /** Number of `statementEnd` events to wait for once the command resolved. */
  expectedEnds: (result: R) => number;
  mock: (handler: (e: QueryEvent) => void) => Promise<R>;
}

/**
 * Run a command that streams `QueryEvent`s (`table_fetch`, `query_execute`, `lease_execute`,
 * `lease_fetch_more`) through a Tauri Channel, the dev bridge's NDJSON stream, or the mock.
 */
export function streamCommand<R>(options: StreamOptions<R>): Promise<R> {
  const { command, args, onEvent, queryId, expectedEnds } = options;
  return streamFetch<R>(command, onEvent, expectedEnds, (handler) => {
    const transport = ipcTransport();
    if (transport === 'mock') return options.mock(handler);
    if (transport === 'bridge') return bridgeStream<R>(command, args, queryId, handler);
    const channel = new Channel<QueryEvent>();
    channel.onmessage = handler;
    return invoke<R>(command, { ...args, onEvent: channel });
  });
}

/** Table fetches always wait for exactly one `statementEnd`. */
export const oneEnd = (): number => 1;

// ---------------------------------------------------------------------------------------------
// Events (job://progress, lease://state, backup://changed)
// ---------------------------------------------------------------------------------------------

interface BridgeSubscriber {
  name: string;
  handler: (payload: unknown) => void;
}

const bridgeSubscribers = new Set<BridgeSubscriber>();
let bridgeEventsAbort: AbortController | null = null;

async function readBridgeEvents(signal: AbortSignal): Promise<void> {
  const res = await fetch(`${bridgeUrl()}/events`, { headers: bridgeHeaders(), signal });
  if (res.status !== 200 || !res.body) throw unreachable(`HTTP ${res.status}`);
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  const dispatch = (line: string) => {
    if (!line.trim()) return;
    let msg: { event?: unknown; payload?: unknown };
    try {
      msg = JSON.parse(line) as typeof msg;
    } catch {
      return;
    }
    if (typeof msg.event !== 'string') return;
    for (const sub of [...bridgeSubscribers]) {
      if (sub.name === msg.event) sub.handler(msg.payload);
    }
  };
  try {
    for (;;) {
      const chunk = await reader.read();
      buffer += decoder.decode(chunk.value, { stream: !chunk.done });
      let newline = buffer.indexOf('\n');
      while (newline >= 0) {
        dispatch(buffer.slice(0, newline));
        buffer = buffer.slice(newline + 1);
        newline = buffer.indexOf('\n');
      }
      if (chunk.done) break;
    }
    dispatch(buffer);
  } finally {
    await reader.cancel().catch(() => undefined);
  }
}

/** Keep one `/events` stream open while anybody listens; reconnect after failures. */
function startBridgeEvents(): void {
  if (bridgeEventsAbort) return;
  const controller = new AbortController();
  bridgeEventsAbort = controller;
  void (async () => {
    while (!controller.signal.aborted) {
      try {
        await readBridgeEvents(controller.signal);
      } catch {
        // unreachable or dropped: retry below
      }
      if (controller.signal.aborted) break;
      await new Promise((r) => setTimeout(r, 1000));
    }
  })();
}

/**
 * Subscribe to a backend event (`job://progress`, `lease://state`, `backup://changed`).
 * Resolves to the function that unsubscribes.
 */
export async function onEvent<T = unknown>(
  name: string,
  handler: (payload: T) => void,
): Promise<() => void> {
  const transport = ipcTransport();
  if (transport === 'mock') return mockListen(name, (p) => handler(p as T));
  if (transport === 'tauri') {
    const unlisten = await listen<T>(name, (e) => handler(e.payload));
    return unlisten;
  }
  const sub: BridgeSubscriber = { name, handler: (p) => handler(p as T) };
  bridgeSubscribers.add(sub);
  startBridgeEvents();
  return () => {
    bridgeSubscribers.delete(sub);
    if (bridgeSubscribers.size === 0) {
      bridgeEventsAbort?.abort();
      bridgeEventsAbort = null;
    }
  };
}

/** Commands of phase 1 plus the small phase 2 additions (URI parsing, diagnostics, SSH trust). */
export const coreApi = {
  appInfo: (): Promise<AppInfo> => call('app_info', () => mockAppInfo),
  listDrivers: (): Promise<DriverDescriptor[]> => call('list_drivers', () => mockDrivers),

  connectionsList: (): Promise<ConnectionsSnapshot> =>
    call('connections_list', () => mock.connectionsList()),
  /** `sshSecret` is the SSH password / key passphrase; omitted = keep the stored one. */
  connectionSave: (
    profile: ConnectionProfile,
    secret: SecretChange,
    sshSecret?: SecretChange,
  ): Promise<ConnectionProfile> =>
    call('connection_save', () => mock.connectionSave(profile, secret), {
      profile,
      secret,
      ...(sshSecret ? { sshSecret } : {}),
    }),
  connectionDelete: (id: Uuid): Promise<void> =>
    call('connection_delete', () => mock.connectionDelete(id), { id }),
  connectionDuplicate: (id: Uuid): Promise<ConnectionProfile> =>
    call('connection_duplicate', () => mock.connectionDuplicate(id), { id }),
  groupSave: (group: ConnectionGroup): Promise<ConnectionGroup> =>
    call('group_save', () => mock.groupSave(group), { group }),
  groupDelete: (id: Uuid): Promise<void> =>
    call('group_delete', () => mock.groupDelete(id), { id }),
  /** `sshSecret` is the SSH password / key passphrase just typed; omitted = use the stored one. */
  connectionTest: (
    profile: ConnectionProfile,
    secret: SecretChange,
    sshSecret?: SecretChange,
  ): Promise<ServerInfo> =>
    call('connection_test', () => mock.connectionTest(profile), {
      profile,
      secret,
      ...(sshSecret ? { sshSecret } : {}),
    }),
  connectionsExport: (path: string, ids?: Uuid[] | null): Promise<number> =>
    call('connections_export', () => mock.connectionsExport(ids ?? null), {
      path,
      ids: ids ?? null,
    }),
  connectionsImport: (path: string): Promise<ConnectionsSnapshot> =>
    call('connections_import', () => mock.connectionsImport(), { path }),
  connectionParseUri: (uri: string): Promise<ParsedConnectionUri> =>
    call('connection_parse_uri', () => mock.connectionParseUri(uri), { uri }),
  driverDiagnostics: (kind: DbKind): Promise<DriverDiagnostics> =>
    call('driver_diagnostics', () => mock.driverDiagnostics(kind), { kind }),
  sshTrustHost: (host: string, port: number, keyType: string, fingerprint: string): Promise<void> =>
    call('ssh_trust_host', () => mock.sshTrustHost(host, port, keyType, fingerprint), {
      host,
      port,
      keyType,
      fingerprint,
    }),

  sessionOpen: (connectionId: Uuid, password?: string | null): Promise<SessionInfo> =>
    call('session_open', () => mock.sessionOpen(connectionId, password ?? null), {
      connectionId,
      password: password ?? null,
    }),
  /** Reconnect a lost session; the session id is kept. */
  sessionReconnect: (sessionId: Uuid, secret?: string | null): Promise<SessionInfo> =>
    call('session_reconnect', () => mock.sessionReconnect(sessionId, secret ?? null), {
      sessionId,
      secret: secret ?? null,
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
  /** Cancels a running query, table fetch or count by its query id (session level). */
  queryCancel: async (sessionId: Uuid, queryId: Uuid): Promise<void> => {
    try {
      await call<void>('query_cancel', () => mock.queryCancel(sessionId, queryId), {
        sessionId,
        queryId,
      });
    } finally {
      // Over the dev bridge, also stop reading the local stream of that fetch.
      abortBridgeStream(queryId);
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
   * Resolves only after BOTH the command returned and `statementEnd` was received (see
   * `streamFetch`).
   */
  tableFetch: (
    sessionId: Uuid,
    request: TableDataRequest,
    onEvent: (e: QueryEvent) => void,
  ): Promise<ExecSummary> =>
    streamCommand<ExecSummary>({
      command: 'table_fetch',
      args: { sessionId, request },
      onEvent,
      queryId: request.queryId,
      expectedEnds: oneEnd,
      mock: (handler) => mock.tableFetch(sessionId, request, handler),
    }),
};
