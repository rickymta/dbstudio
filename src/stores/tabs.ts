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

/** Payload of a `debugSource` tab: the source of a routine the SQL debugger stopped in. */
export interface DebugSourcePayload {
  sessionId: Uuid;
  connectionId: Uuid;
  signature: string;
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
  | (TabBase & { kind: 'debugSource'; payload: DebugSourcePayload })
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

/** The tabs of one scope (workspace) with its active tab. */
export interface TabSet {
  tabs: TabItem[];
  activeId: string;
}

/** Scope of the tabs shown when no workspace is active. */
export const NO_SCOPE = '';

const emptySet = (): TabSet => ({ tabs: [WELCOME_TAB], activeId: WELCOME_TAB.id });

interface TabsState {
  /** Tabs of the current scope, the ones on screen. */
  tabs: TabItem[];
  activeId: string;
  /** Workspace id whose tabs are on screen ([`NO_SCOPE`] for none). */
  scope: string;
  /**
   * Tab sets of the other scopes. Their tabs stay alive (editor text, pending edits, leases):
   * code that cleans up after closed tabs must look at [`allTabs`], not only `tabs`.
   */
  stashed: Record<string, TabSet>;
  /** Show the tab set of `scope` (a new one has only the welcome tab); the current one is kept. */
  switchScope: (scope: string) => void;
  /**
   * A workspace was deleted: its tabs join the tabs of no workspace (nothing is closed). When it
   * is the current scope, no workspace becomes current.
   */
  dropScope: (scope: string) => void;
  /** Move tabs of the current scope into the stashed set of `scope` (restored editor tabs). */
  moveToScope: (ids: string[], scope: string) => void;
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
  /** Point an editor tab at another connection (and its session, `null` while not connected). */
  rebindEditor: (
    tabId: string,
    target: { connectionId: Uuid | null; sessionId: Uuid | null },
  ) => void;
}

function afterClose(tabs: TabItem[], activeId: string, removedIndexes: number[]): string {
  if (tabs.some((t) => t.id === activeId)) return activeId;
  const first = Math.min(...removedIndexes);
  return (tabs[Math.max(0, first - 1)] ?? tabs[0] ?? WELCOME_TAB).id;
}

/** Every tab of every scope: the ones on screen first, then the stashed ones. */
export function allTabs(state: Pick<TabsState, 'tabs' | 'stashed'>): TabItem[] {
  const out = [...state.tabs];
  for (const set of Object.values(state.stashed)) out.push(...set.tabs);
  return out;
}

/** Scope holding the tab `id`, or `undefined` when no scope has it. */
export function scopeOfTab(state: Pick<TabsState, 'tabs' | 'stashed' | 'scope'>, id: string) {
  if (state.tabs.some((t) => t.id === id)) return state.scope;
  return Object.entries(state.stashed).find(([, s]) => s.tabs.some((t) => t.id === id))?.[0];
}

/** `extra` appended to `tabs`, skipping ids already there (the welcome tab is shared). */
function mergeTabs(tabs: TabItem[], extra: TabItem[]): TabItem[] {
  const ids = new Set(tabs.map((t) => t.id));
  return [...tabs, ...extra.filter((t) => !ids.has(t.id))];
}

/** `closeSessionTabs` on one tab set; `removed` lists the indexes of the tabs dropped. */
function withoutSession(tabs: TabItem[], sessionId: Uuid) {
  const owned = (t: TabItem) => t.payload?.sessionId === sessionId;
  const removed = tabs
    .map((t, i) => (t.kind !== 'editor' && owned(t) ? i : -1))
    .filter((i) => i >= 0);
  const detached = tabs.some((t) => t.kind === 'editor' && owned(t));
  const next = tabs
    .filter((t) => t.kind === 'editor' || !owned(t))
    .map((t) =>
      t.kind === 'editor' && owned(t) ? { ...t, payload: { ...t.payload, sessionId: null } } : t,
    );
  return { tabs: next, removed, changed: removed.length > 0 || detached };
}

export const useTabsStore = create<TabsState>((set, get) => ({
  tabs: [WELCOME_TAB],
  activeId: WELCOME_TAB.id,
  scope: NO_SCOPE,
  stashed: {},
  switchScope: (scope) =>
    set((s) => {
      if (scope === s.scope) return s;
      const stashed = { ...s.stashed, [s.scope]: { tabs: s.tabs, activeId: s.activeId } };
      const next = stashed[scope] ?? emptySet();
      delete stashed[scope];
      return { scope, tabs: next.tabs, activeId: next.activeId, stashed };
    }),
  dropScope: (scope) =>
    set((s) => {
      if (scope === NO_SCOPE) return s;
      if (scope === s.scope) {
        const base = s.stashed[NO_SCOPE] ?? emptySet();
        const stashed = { ...s.stashed };
        delete stashed[NO_SCOPE];
        return {
          scope: NO_SCOPE,
          tabs: mergeTabs(base.tabs, s.tabs),
          activeId: s.activeId,
          stashed,
        };
      }
      const dropped = s.stashed[scope];
      if (!dropped) return s;
      const stashed = { ...s.stashed };
      delete stashed[scope];
      if (s.scope === NO_SCOPE) return { stashed, tabs: mergeTabs(s.tabs, dropped.tabs) };
      const base = stashed[NO_SCOPE] ?? emptySet();
      stashed[NO_SCOPE] = { ...base, tabs: mergeTabs(base.tabs, dropped.tabs) };
      return { stashed };
    }),
  moveToScope: (ids, scope) =>
    set((s) => {
      if (scope === s.scope) return s;
      const moving = s.tabs.filter((t) => ids.includes(t.id) && t.closable);
      if (moving.length === 0) return s;
      const tabs = s.tabs.filter((t) => !moving.includes(t));
      const target = s.stashed[scope] ?? emptySet();
      return {
        tabs,
        activeId: tabs.some((t) => t.id === s.activeId) ? s.activeId : (tabs[0] ?? WELCOME_TAB).id,
        stashed: { ...s.stashed, [scope]: { ...target, tabs: mergeTabs(target.tabs, moving) } },
      };
    }),
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
  rebindEditor: (tabId, target) =>
    set((s) => ({
      tabs: s.tabs.map((t) =>
        t.id === tabId && t.kind === 'editor'
          ? {
              ...t,
              payload: {
                ...t.payload,
                connectionId: target.connectionId,
                sessionId: target.sessionId,
              },
            }
          : t,
      ),
    })),
  closeSessionTabs: (sessionId) =>
    set((s) => {
      // Sessions are shared by every workspace: the stashed tab sets lose its tabs too.
      let stashedChanged = false;
      const stashed: Record<string, TabSet> = {};
      for (const [scope, tabSet] of Object.entries(s.stashed)) {
        const r = withoutSession(tabSet.tabs, sessionId);
        stashedChanged ||= r.changed;
        stashed[scope] = r.changed
          ? {
              tabs: r.tabs,
              activeId: r.removed.length
                ? afterClose(r.tabs, tabSet.activeId, r.removed)
                : tabSet.activeId,
            }
          : tabSet;
      }
      const current = withoutSession(s.tabs, sessionId);
      if (!current.changed && !stashedChanged) return s;
      return {
        tabs: current.tabs,
        activeId:
          current.removed.length > 0
            ? afterClose(current.tabs, s.activeId, current.removed)
            : s.activeId,
        stashed: stashedChanged ? stashed : s.stashed,
      };
    }),
}));
