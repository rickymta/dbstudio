import { Channel, invoke } from '@tauri-apps/api/core';
import { mock, mockAppInfo, mockDrivers } from './mock';
import type {
  AppInfo,
  ConnectionGroup,
  ConnectionProfile,
  ConnectionsSnapshot,
  DriverDescriptor,
  ExecSummary,
  IpcError,
  ObjectNode,
  ObjectPath,
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

/**
 * Call a backend command, or the mock when running outside Tauri. Errors are normalized to
 * [`IpcError`]. All backend access must go through this module.
 */
async function call<T>(
  command: string,
  mockFn: () => T | Promise<T>,
  args?: Record<string, unknown> | (() => Record<string, unknown>),
): Promise<T> {
  try {
    if (!isTauri()) return await mockFn();
    return await invoke<T>(command, typeof args === 'function' ? args() : args);
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
  objectDdl: (sessionId: Uuid, path: ObjectPath): Promise<string> =>
    call('object_ddl', () => mock.objectDdl(sessionId, path), { sessionId, path }),
  tableDetails: (sessionId: Uuid, path: ObjectPath): Promise<TableDetails> =>
    call('table_details', () => mock.tableDetails(sessionId, path), { sessionId, path }),
  tableCount: (sessionId: Uuid, request: TableCountRequest): Promise<number> =>
    call('table_count', () => mock.tableCount(sessionId, request), { sessionId, request }),
  queryCancel: (sessionId: Uuid, queryId: Uuid): Promise<void> =>
    call('query_cancel', () => mock.queryCancel(sessionId, queryId), { sessionId, queryId }),

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
      if (!isTauri()) return mock.tableFetch(sessionId, request, handler);
      const channel = new Channel<QueryEvent>();
      channel.onmessage = handler;
      return invoke<ExecSummary>('table_fetch', { sessionId, request, onEvent: channel });
    }),
};
