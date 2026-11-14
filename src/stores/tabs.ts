import { create } from 'zustand';
import { pathKey } from '@/lib/objectPath';
import type { ObjectPath, Uuid } from '@/lib/types';
import { getTabGuard } from './tabGuard';

export { useTabGuard, type TabGuard } from './tabGuard';

/** Plain string, or an i18n key (`ns` defaults to `common`). */
export type TabTitle = string | { key: string; ns?: string };

interface TabBase {
  id: string;
  title: TabTitle;
  closable: boolean;
  pinned?: boolean;
}

/** Payload of a `table` or `ddl` tab. */
export interface ObjectTabPayload {
  sessionId: Uuid;
  connectionId: Uuid;
  path: ObjectPath;
}
export type TableTabPayload = ObjectTabPayload;
export type DdlTabPayload = ObjectTabPayload;

/** What an editor tab talks to. */
export type EditorMode = 'sql' | 'mongo' | 'redis';

/**
 * Payload of an `editor` tab. The text itself lives in the editor store / `editor_tabs`;
 * `sessionId` is `null` once the session closed (the tab keeps its text).
 */
export interface EditorTabPayload {
  tabStateId: Uuid;
  connectionId: Uuid | null;
  sessionId: Uuid | null;
  mode: EditorMode;
}

/** Payload of a `plan` tab; the plan itself is kept in the plan store. */
export interface PlanTabPayload {
  sessionId: Uuid;
  connectionId: Uuid;
  planId: string;
}

/** Payload of an `erd` tab; `layoutId` is `null` for a diagram that was not saved yet. */
export interface ErdTabPayload {
  sessionId: Uuid;
  connectionId: Uuid;
  scope: ObjectPath;
  layoutId: Uuid | null;
}

/** Payload of a Redis `key` tab. */
export type KeyTabPayload = ObjectTabPayload;

export type TabItem =
  | (TabBase & { kind: 'welcome'; payload?: undefined })
  | (TabBase & { kind: 'table'; payload: TableTabPayload })
  | (TabBase & { kind: 'ddl'; payload: DdlTabPayload })
  | (TabBase & { kind: 'editor'; payload: EditorTabPayload })
  | (TabBase & { kind: 'plan'; payload: PlanTabPayload })
  | (TabBase & { kind: 'erd'; payload: ErdTabPayload })
  | (TabBase & { kind: 'key'; payload: KeyTabPayload })
  | (TabBase & { kind: 'backup'; payload?: undefined });

export type TabKind = TabItem['kind'];

export const WELCOME_TAB: TabItem = {
  id: 'welcome',
  kind: 'welcome',
  title: { key: 'tabs.welcome' },
  closable: false,
};

/** Stable tab id for the data tab of an object. */
export const tableTabId = (sessionId: Uuid, path: ObjectPath) =>
  `table:${sessionId}:${pathKey(path)}`;
/** Stable tab id for the DDL tab of an object. */
export const ddlTabId = (sessionId: Uuid, path: ObjectPath) => `ddl:${sessionId}:${pathKey(path)}`;

/** The backup tab is a singleton. */
export const BACKUP_TAB_ID = 'backup';
export const editorTabId = (tabStateId: Uuid) => `editor:${tabStateId}`;
export const planTabId = (planId: string) => `plan:${planId}`;
export const erdTabId = (sessionId: Uuid, scope: ObjectPath) =>
  `erd:${sessionId}:${pathKey(scope)}`;
export const keyTabId = (sessionId: Uuid, path: ObjectPath) => `key:${sessionId}:${pathKey(path)}`;

interface TabsState {
  tabs: TabItem[];
  activeId: string;
  /** Add the tab and activate it; if a tab with the same id exists, only activate it. */
  openTab: (tab: TabItem) => void;
  /** Close a closable tab; the neighbor to the left (else right) becomes active. */
  closeTab: (id: string) => void;
  /**
   * Close like the user asked: the tab's guard (see `useTabGuard`) may veto. Resolves to
   * whether the tab was closed. Tabs without a guard close synchronously.
   */
  requestClose: (id: string) => Promise<boolean>;
  activate: (id: string) => void;
  togglePin: (id: string) => void;
  /**
   * Close every tab whose payload belongs to `sessionId`. `editor` tabs stay open (their text
   * must survive) and only lose their `sessionId`.
   */
  closeSessionTabs: (sessionId: Uuid) => void;
}

function afterClose(tabs: TabItem[], activeId: string, removedIndexes: number[]): string {
  if (tabs.some((t) => t.id === activeId)) return activeId;
  const first = Math.min(...removedIndexes);
  return (tabs[Math.max(0, first - 1)] ?? tabs[0] ?? WELCOME_TAB).id;
}

export const useTabsStore = create<TabsState>((set, get) => ({
  tabs: [WELCOME_TAB],
  activeId: WELCOME_TAB.id,
  openTab: (tab) =>
    set((s) =>
      s.tabs.some((t) => t.id === tab.id)
        ? { activeId: tab.id }
        : { tabs: [...s.tabs, tab], activeId: tab.id },
    ),
  closeTab: (id) =>
    set((s) => {
      const index = s.tabs.findIndex((t) => t.id === id);
      if (index < 0 || !s.tabs[index]!.closable) return s;
      const tabs = s.tabs.filter((t) => t.id !== id);
      return { tabs, activeId: afterClose(tabs, s.activeId, [index]) };
    }),
  requestClose: (id) => {
    const guard = getTabGuard(id);
    if (!guard) {
      get().closeTab(id);
      return Promise.resolve(true);
    }
    return Promise.resolve(guard()).then((allowed) => {
      if (allowed) get().closeTab(id);
      return allowed;
    });
  },
  togglePin: (id) =>
    set((s) => ({ tabs: s.tabs.map((t) => (t.id === id ? { ...t, pinned: !t.pinned } : t)) })),
  activate: (id) => set((s) => (s.tabs.some((t) => t.id === id) ? { activeId: id } : s)),
  closeSessionTabs: (sessionId) =>
    set((s) => {
      const owned = (t: TabItem) => t.payload?.sessionId === sessionId;
      const detached = s.tabs.some((t) => t.kind === 'editor' && owned(t));
      const removed = s.tabs
        .map((t, i) => (t.kind !== 'editor' && owned(t) ? i : -1))
        .filter((i) => i >= 0);
      if (removed.length === 0 && !detached) return s;
      const tabs = s.tabs
        .filter((t) => t.kind === 'editor' || !owned(t))
        .map((t) =>
          t.kind === 'editor' && owned(t)
            ? { ...t, payload: { ...t.payload, sessionId: null } }
            : t,
        );
      return {
        tabs,
        activeId: removed.length > 0 ? afterClose(tabs, s.activeId, removed) : s.activeId,
      };
    }),
}));
