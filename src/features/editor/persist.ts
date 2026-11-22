// Editor tabs survive a reload (QUERY-01): the open tabs are written through `editor_tabs_set`
// and read back at start-up. Leases are not restored: a restored tab reconnects on first use.

import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import type { DbKind, EditorTabState } from '@/lib/types';
import { allTabs, NO_SCOPE, useTabsStore, type EditorMode, type TabItem } from '@/stores/tabs';
import { addEditorTab } from './open';
import { getTabData, isDirty, useEditorStore, type EditorTabData } from './editorStore';
import { applyDebugState, debugStateOf, onDebugStateChanged } from './persistHooks';

export function modeOfKind(kind: DbKind | undefined): EditorMode {
  if (kind === 'mongodb') return 'mongo';
  if (kind === 'redis') return 'redis';
  return 'sql';
}

function titleText(tab: TabItem): string {
  return typeof tab.title === 'string'
    ? tab.title
    : i18n.t(tab.title.key, { ns: tab.title.ns ?? 'common' });
}

/** The state to persist for the open editor tabs, in tab order. */
export function collectEditorTabs(): EditorTabState[] {
  const states: EditorTabState[] = [];
  // Editors of every workspace, not only the one on screen.
  for (const tab of allTabs(useTabsStore.getState())) {
    if (tab.kind !== 'editor') continue;
    const data = getTabData(tab.payload.tabStateId);
    if (!data) continue;
    states.push({
      id: tab.payload.tabStateId,
      position: states.length,
      title: data.title || titleText(tab),
      connectionId: tab.payload.connectionId,
      database: data.leaseState?.database ?? data.database,
      schema: data.leaseState?.schema ?? data.schema,
      text: data.text,
      filePath: data.filePath,
      cursor: data.cursor,
      autoCommit: data.leaseState?.autoCommit ?? data.autoCommit,
      debug: debugStateOf(tab.payload.tabStateId),
    });
  }
  return states;
}

const DIRTY_KEY = 'dbstudio.editor.dirtyTabs';
const SCOPES_KEY = 'dbstudio.editor.tabScopes';

/** Workspace of each editor tab that does not belong to "no workspace" (`EditorTabState` has none). */
export function editorTabScopes(): Record<string, string> {
  const { tabs, scope, stashed } = useTabsStore.getState();
  const out: Record<string, string> = {};
  const add = (list: TabItem[], s: string) => {
    if (s === NO_SCOPE) return;
    for (const t of list) if (t.kind === 'editor') out[t.payload.tabStateId] = s;
  };
  add(tabs, scope);
  for (const [s, set] of Object.entries(stashed)) add(set.tabs, s);
  return out;
}

function readScopes(): Record<string, string> {
  try {
    const raw: unknown = JSON.parse(localStorage.getItem(SCOPES_KEY) ?? '{}');
    if (!raw || typeof raw !== 'object') return {};
    return Object.fromEntries(
      Object.entries(raw).filter((e): e is [string, string] => typeof e[1] === 'string'),
    );
  } catch {
    return {};
  }
}

function writeScopes(scopes: Record<string, string>): void {
  try {
    localStorage.setItem(SCOPES_KEY, JSON.stringify(scopes));
  } catch {
    // storage unavailable: restored tabs then open in the current workspace
  }
}

/** Ids of file-backed tabs with unsaved changes (`EditorTabState` has no field for it). */
function readDirty(): Set<string> {
  try {
    const raw = localStorage.getItem(DIRTY_KEY);
    return new Set(raw ? (JSON.parse(raw) as string[]) : []);
  } catch {
    return new Set();
  }
}

function writeDirty(ids: string[]): void {
  try {
    localStorage.setItem(DIRTY_KEY, JSON.stringify(ids));
  } catch {
    // storage unavailable: the unsaved mark is just not kept
  }
}

/**
 * Reopen the persisted tabs (once per app start). Resolves to how many were restored, or `null`
 * when the saved tabs could not be read (the caller must then not start saving).
 */
export async function restoreEditorTabs(): Promise<number | null> {
  let states: EditorTabState[];
  try {
    states = await ipc.editorTabsGet();
  } catch {
    return null;
  }
  // Connections only decide the mode; without them every tab opens as SQL.
  let kinds = new Map<string, DbKind>();
  try {
    kinds = new Map((await ipc.connectionsList()).profiles.map((p) => [p.id, p.kind]));
  } catch {
    // keep the empty map
  }
  const dirty = readDirty();
  const open = new Set(allTabs(useTabsStore.getState()).map((t) => t.id));
  const scopes = readScopes();
  const byScope = new Map<string, string[]>();
  const previous = useTabsStore.getState().activeId;
  let restored = 0;
  for (const state of [...states].sort((a, b) => a.position - b.position)) {
    if (open.has(`editor:${state.id}`)) continue;
    const cursor = Math.min(Math.max(state.cursor, 0), state.text.length);
    addEditorTab(
      {
        tabStateId: state.id,
        connectionId: state.connectionId,
        sessionId: null,
        mode: modeOfKind(state.connectionId ? kinds.get(state.connectionId) : undefined),
      },
      {
        title: state.title,
        text: state.text,
        cursor,
        selection: { from: cursor, to: cursor },
        filePath: state.filePath,
        // The saved text is unknown after a reload: remember only whether it differed.
        savedText: state.filePath && !dirty.has(state.id) ? state.text : null,
        restoredDirty: !!state.filePath && dirty.has(state.id),
        database: state.database,
        schema: state.schema,
        autoCommit: state.autoCommit,
      },
      false,
    );
    if (state.debug) applyDebugState(state.id, state.debug);
    const scope = scopes[state.id] ?? NO_SCOPE;
    byScope.set(scope, [...(byScope.get(scope) ?? []), `editor:${state.id}`]);
    restored++;
  }
  // Each tab goes back to the workspace it was in (the tab set of that workspace).
  for (const [scope, ids] of byScope) useTabsStore.getState().moveToScope(ids, scope);
  useTabsStore.getState().activate(previous);
  return restored;
}

let restoredOnce: Promise<number | null> | null = null;

/** `restoreEditorTabs` once per app start, however many times the host mounts. */
export function restoreEditorTabsOnce(): Promise<number | null> {
  restoredOnce ??= restoreEditorTabs();
  return restoredOnce;
}

/** Test helper: allow the next `restoreEditorTabsOnce` to restore again. */
export function resetRestoreOnce(): void {
  restoredOnce = null;
}

const SAVE_DELAY_MS = 500;

/** Whether anything that is persisted differs between two editor states. */
function persistedChange(a: EditorTabData | undefined, b: EditorTabData): boolean {
  return (
    !a ||
    a.text !== b.text ||
    a.cursor !== b.cursor ||
    a.title !== b.title ||
    a.filePath !== b.filePath ||
    a.savedText !== b.savedText ||
    a.database !== b.database ||
    a.schema !== b.schema ||
    a.autoCommit !== b.autoCommit ||
    a.leaseState?.database !== b.leaseState?.database ||
    a.leaseState?.schema !== b.leaseState?.schema ||
    a.leaseState?.autoCommit !== b.leaseState?.autoCommit
  );
}

function dirtyIds(states: EditorTabState[]): string[] {
  return states
    .filter((s) => {
      const d = getTabData(s.id);
      return d ? isDirty(d) : false;
    })
    .map((s) => s.id);
}

function signatureNow(): string {
  const states = collectEditorTabs();
  return JSON.stringify(states) + dirtyIds(states).join(',') + JSON.stringify(editorTabScopes());
}

/**
 * Write the editor tabs whenever they change (debounced). Returns a function that stops and
 * flushes. Call only after a successful `restoreEditorTabs`, or the restore could be overwritten.
 */
export function startEditorPersistence(): () => void {
  let last = signatureNow();
  let timer: ReturnType<typeof setTimeout> | null = null;
  const flush = () => {
    timer = null;
    const states = collectEditorTabs();
    const dirty = dirtyIds(states);
    const scopes = editorTabScopes();
    const signature = JSON.stringify(states) + dirty.join(',') + JSON.stringify(scopes);
    if (signature === last) return;
    last = signature;
    writeDirty(dirty);
    writeScopes(scopes);
    ipc.editorTabsSet(states).catch(() => undefined);
  };
  const schedule = () => {
    timer ??= setTimeout(flush, SAVE_DELAY_MS);
  };
  const stopTabs = useTabsStore.subscribe(schedule);
  const stopDebug = onDebugStateChanged(schedule);
  // Run results and lease progress change the store constantly; only persisted fields count.
  const stopEditors = useEditorStore.subscribe((state, prev) => {
    for (const [id, data] of Object.entries(state.tabs)) {
      if (persistedChange(prev.tabs[id], data)) return schedule();
    }
    if (Object.keys(prev.tabs).some((id) => !(id in state.tabs))) schedule();
  });
  return () => {
    stopTabs();
    stopDebug();
    stopEditors();
    if (timer) clearTimeout(timer);
    flush();
  };
}
