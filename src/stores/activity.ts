// Activity log: what the application did (connect, run, apply changes, export, backup...), with
// its outcome and duration. Fed by the IPC layer (`trackCommand`) and by a few explicit `log`
// calls; shown by the Activity Logs window. Statement text, values and secrets are never logged.
//
// This module must not import `@/lib/ipc`: the IPC layer imports it.

import { create } from 'zustand';

export type ActivityStatus = 'started' | 'finished' | 'failed' | 'info';

/** One line of the Activity Logs window. */
export interface Activity {
  id: number;
  /** Connection the event belongs to; null = application-level. */
  connectionId: string | null;
  /** Key under `activity:event.*`. */
  event: string;
  status: ActivityStatus;
  /** Epoch milliseconds. */
  at: number;
  /** Duration, set with `finished` / `failed`. */
  ms?: number;
  error?: string;
}

/** Entries kept in memory and in localStorage. */
export const ACTIVITY_MAX = 1000;
/** Entries older than this are dropped when the log is loaded. */
export const ACTIVITY_RETENTION_MS = 30 * 24 * 60 * 60 * 1000;
const STORAGE_KEY = 'dbstudio.activity';
const SAVE_DELAY_MS = 800;

function load(now = Date.now()): Activity[] {
  try {
    const raw: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? '[]');
    if (!Array.isArray(raw)) return [];
    return raw
      .filter(
        (e): e is Activity =>
          !!e &&
          typeof e.at === 'number' &&
          typeof e.event === 'string' &&
          typeof e.status === 'string' &&
          now - e.at < ACTIVITY_RETENTION_MS,
      )
      .slice(-ACTIVITY_MAX)
      .map((e, i) => ({ ...e, connectionId: e.connectionId ?? null, id: i + 1 }));
  } catch {
    return [];
  }
}

let saveTimer: ReturnType<typeof setTimeout> | undefined;
function scheduleSave(): void {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(useActivity.getState().items));
    } catch {
      // storage unavailable or full: the log stays in memory
    }
  }, SAVE_DELAY_MS);
}

interface ActivityStore {
  items: Activity[];
  log: (entry: Omit<Activity, 'id' | 'at'>) => void;
  /** Log `started`; the returned function logs `finished` or `failed` with the duration. */
  begin: (connectionId: string | null, event: string) => (error?: string) => void;
  /** Remove the entries of one connection, of the application (`null`) or all of them. */
  clear: (scope: string | null | 'all') => void;
}

const initial = load();
let nextId = initial.length + 1;

export const useActivity = create<ActivityStore>((set, get) => ({
  items: initial,
  log: (entry) => {
    const item: Activity = { ...entry, id: nextId++, at: Date.now() };
    set((s) => ({ items: [...s.items.slice(-(ACTIVITY_MAX - 1)), item] }));
    scheduleSave();
  },
  begin: (connectionId, event) => {
    const start = performance.now();
    get().log({ connectionId, event, status: 'started' });
    return (error) =>
      get().log({
        connectionId,
        event,
        status: error ? 'failed' : 'finished',
        ms: Math.round(performance.now() - start),
        ...(error ? { error } : {}),
      });
  },
  clear: (scope) => {
    set((s) => ({
      items: scope === 'all' ? [] : s.items.filter((i) => i.connectionId !== scope),
    }));
    scheduleSave();
  },
}));

// -- IPC commands -----------------------------------------------------------------------------

/**
 * Commands worth a line in the log, by `activity:event.*` key. Reads (tree, grid pages, lists)
 * and high-frequency calls are left out on purpose.
 */
const TRACKED: Record<string, string> = {
  session_open: 'connect',
  session_close: 'disconnect',
  session_reconnect: 'reconnect',
  connection_test: 'testConnection',
  connection_save: 'saveConnection',
  connection_delete: 'deleteConnection',
  connection_duplicate: 'duplicateConnection',
  connections_import: 'importConnections',
  connections_export: 'exportConnections',
  ssh_trust_host: 'trustHostKey',
  lease_execute: 'runStatements',
  lease_begin: 'beginTransaction',
  lease_commit: 'commit',
  lease_rollback: 'rollback',
  changes_apply: 'applyChanges',
  plan_explain: 'explain',
  export_start: 'startExport',
  import_start: 'startImport',
  backup_job_run_now: 'runBackup',
  backup_job_save: 'saveBackupJob',
  backup_job_delete: 'deleteBackupJob',
  backup_verify: 'verifyBackup',
  restore_start: 'startRestore',
  job_cancel: 'cancelJob',
  debug_start: 'startDebug',
  debug_stop: 'stopDebug',
  settings_set: 'saveSettings',
  history_clear: 'clearHistory',
  plan_history_clear: 'clearPlanHistory',
};

/** Session / lease → connection, learnt from the results that carry both ids. */
const connectionOf = new Map<string, string>();

const str = (v: unknown): string | undefined => (typeof v === 'string' && v ? v : undefined);

function learn(args: Record<string, unknown> | undefined, result: unknown): void {
  if (!result || typeof result !== 'object') return;
  const r = result as Record<string, unknown>;
  const connectionId =
    str(r.connectionId) ??
    connectionOf.get(str(r.sessionId) ?? '') ??
    connectionOf.get(str(args?.sessionId) ?? '');
  if (!connectionId) return;
  for (const id of [str(r.sessionId), str(r.leaseId), str(r.debugId)]) {
    if (id) connectionOf.set(id, connectionId);
  }
}

function connectionFor(args: Record<string, unknown> | undefined): string | null {
  if (!args) return null;
  const profile = args.profile as { id?: unknown } | undefined;
  return (
    str(args.connectionId) ??
    str(profile?.id) ??
    connectionOf.get(str(args.sessionId) ?? '') ??
    connectionOf.get(str(args.leaseId) ?? '') ??
    connectionOf.get(str(args.debugId) ?? '') ??
    null
  );
}

const messageOf = (e: unknown): string => {
  const message = (e as { message?: unknown } | null)?.message;
  return typeof message === 'string' && message ? message : String(e);
};

/**
 * Run an IPC command, logging it when it is one of the tracked ones. Results are also used to
 * learn which connection a session, lease or debug session belongs to.
 */
export async function trackCommand<T>(
  command: string,
  args: Record<string, unknown> | undefined,
  run: () => Promise<T>,
): Promise<T> {
  const event = TRACKED[command];
  const done = event ? useActivity.getState().begin(connectionFor(args), event) : null;
  try {
    const result = await run();
    learn(args, result);
    done?.();
    return result;
  } catch (e) {
    // A cancelled prompt is not a failure worth a red line.
    if ((e as { kind?: unknown } | null)?.kind === 'cancelled') done?.();
    else done?.(messageOf(e));
    throw e;
  }
}

/** Test helper: empty the log and forget what was learnt. */
export function resetActivity(): void {
  clearTimeout(saveTimer);
  connectionOf.clear();
  nextId = 1;
  useActivity.setState({ items: [] });
  try {
    localStorage.removeItem(STORAGE_KEY);
  } catch {
    // storage unavailable
  }
}

export const activityStorage = { key: STORAGE_KEY, load };
