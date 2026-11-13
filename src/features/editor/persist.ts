// Editor tabs survive a reload (QUERY-01): the open tabs are written through `editor_tabs_set`
// and read back at start-up. Leases are not restored: a restored tab reconnects on first use.

import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import type { DbKind, EditorTabState } from '@/lib/types';
import { useTabsStore, type EditorMode, type TabItem } from '@/stores/tabs';
import { addEditorTab } from './open';
import { getTabData, useEditorStore } from './editorStore';

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
  for (const tab of useTabsStore.getState().tabs) {
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
    });
  }
  return states;
}

/** Reopen the persisted tabs (once per app start). Resolves to how many were restored. */
export async function restoreEditorTabs(): Promise<number> {
  let states: EditorTabState[];
  let kinds: Map<string, DbKind>;
  try {
    states = await ipc.editorTabsGet();
    kinds = new Map((await ipc.connectionsList()).profiles.map((p) => [p.id, p.kind]));
  } catch {
    return 0;
  }
  const open = new Set(useTabsStore.getState().tabs.map((t) => t.id));
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
        savedText: state.filePath ? state.text : null,
        database: state.database,
        schema: state.schema,
        autoCommit: state.autoCommit,
      },
      false,
    );
    restored++;
  }
  useTabsStore.getState().activate(previous);
  return restored;
}

let restoredOnce: Promise<number> | null = null;

/** `restoreEditorTabs` once per app start, however many times the host mounts. */
export function restoreEditorTabsOnce(): Promise<number> {
  restoredOnce ??= restoreEditorTabs();
  return restoredOnce;
}

/** Test helper: allow the next `restoreEditorTabsOnce` to restore again. */
export function resetRestoreOnce(): void {
  restoredOnce = null;
}

const SAVE_DELAY_MS = 500;

/**
 * Write the editor tabs whenever they change (debounced). Returns a function that stops and
 * flushes. Call after `restoreEditorTabs`, or the restore would be overwritten by an empty list.
 */
export function startEditorPersistence(): () => void {
  let last = JSON.stringify(collectEditorTabs());
  let timer: ReturnType<typeof setTimeout> | null = null;
  const flush = () => {
    timer = null;
    const states = collectEditorTabs();
    const signature = JSON.stringify(states);
    if (signature === last) return;
    last = signature;
    ipc.editorTabsSet(states).catch(() => undefined);
  };
  const schedule = () => {
    timer ??= setTimeout(flush, SAVE_DELAY_MS);
  };
  const stopTabs = useTabsStore.subscribe(schedule);
  const stopEditors = useEditorStore.subscribe(schedule);
  return () => {
    stopTabs();
    stopEditors();
    if (timer) clearTimeout(timer);
    flush();
  };
}
