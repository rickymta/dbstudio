import { create } from 'zustand';
import { pathKey } from '@/lib/objectPath';
import type { ObjectPath, Uuid } from '@/lib/types';

/** Plain string, or an i18n key (`ns` defaults to `common`). */
export type TabTitle = string | { key: string; ns?: string };

interface TabBase {
  id: string;
  title: TabTitle;
  closable: boolean;
}

/** Payload of a `table` or `ddl` tab. */
export interface ObjectTabPayload {
  sessionId: Uuid;
  connectionId: Uuid;
  path: ObjectPath;
}
export type TableTabPayload = ObjectTabPayload;
export type DdlTabPayload = ObjectTabPayload;

export type TabItem =
  | (TabBase & { kind: 'welcome'; payload?: undefined })
  | (TabBase & { kind: 'table'; payload: TableTabPayload })
  | (TabBase & { kind: 'ddl'; payload: DdlTabPayload });

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

interface TabsState {
  tabs: TabItem[];
  activeId: string;
  /** Add the tab and activate it; if a tab with the same id exists, only activate it. */
  openTab: (tab: TabItem) => void;
  /** Close a closable tab; the neighbor to the left (else right) becomes active. */
  closeTab: (id: string) => void;
  activate: (id: string) => void;
  /** Close every tab whose payload belongs to `sessionId`. */
  closeSessionTabs: (sessionId: Uuid) => void;
}

function afterClose(tabs: TabItem[], activeId: string, removedIndexes: number[]): string {
  if (tabs.some((t) => t.id === activeId)) return activeId;
  const first = Math.min(...removedIndexes);
  return (tabs[Math.max(0, first - 1)] ?? tabs[0] ?? WELCOME_TAB).id;
}

export const useTabsStore = create<TabsState>((set) => ({
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
  activate: (id) => set((s) => (s.tabs.some((t) => t.id === id) ? { activeId: id } : s)),
  closeSessionTabs: (sessionId) =>
    set((s) => {
      const removed = s.tabs
        .map((t, i) => (t.payload?.sessionId === sessionId ? i : -1))
        .filter((i) => i >= 0);
      if (removed.length === 0) return s;
      const tabs = s.tabs.filter((t) => t.payload?.sessionId !== sessionId);
      return { tabs, activeId: afterClose(tabs, s.activeId, removed) };
    }),
}));
