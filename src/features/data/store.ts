// Per-tab state of the table tab, kept alive while the tab exists (the tab registry only renders
// the active tab, so component state would be lost on every tab switch).
//
// - `useDataUiStore`: small UI state (sub-tab, selection, viewer, column widths, filter draft).
// - `getGridModel`: the row store of a tab (plain class, see gridModel.ts).
// Both are dropped when the tab is closed (see the `useTabsStore` subscription below).

import { create } from 'zustand';
import type { ObjectPath, Uuid } from '@/lib/types';
import { useTabsStore } from '@/stores/tabs';
import { GridModel } from './gridModel';
import { emptyDraft, type FilterDraft } from './request';

export type SubTab = 'data' | 'structure' | 'ddl';
export type ViewerPosition = 'right' | 'bottom';

export interface CellPos {
  row: number;
  col: number;
}

export interface TabUiState {
  subTab: SubTab;
  selection: CellPos | null;
  viewerOpen: boolean;
  viewerPosition: ViewerPosition;
  /** Viewer size in px (width when right, height when bottom). */
  viewerSize: number;
  /** User-resized column widths by column name. */
  colWidths: Record<string, number>;
  draft: FilterDraft;
}

export function initialTabUi(): TabUiState {
  return {
    subTab: 'data',
    selection: null,
    viewerOpen: false,
    viewerPosition: 'right',
    viewerSize: 360,
    colWidths: {},
    draft: emptyDraft(),
  };
}

const DEFAULT_UI = initialTabUi();

interface DataUiStore {
  byTab: Record<string, TabUiState>;
  patch: (tabId: string, patch: Partial<TabUiState>) => void;
  drop: (tabId: string) => void;
}

export const useDataUiStore = create<DataUiStore>((set) => ({
  byTab: {},
  patch: (tabId, patch) =>
    set((s) => ({
      byTab: { ...s.byTab, [tabId]: { ...(s.byTab[tabId] ?? initialTabUi()), ...patch } },
    })),
  drop: (tabId) =>
    set((s) => {
      if (!(tabId in s.byTab)) return s;
      const byTab = { ...s.byTab };
      delete byTab[tabId];
      return { byTab };
    }),
}));

/** UI state of a tab plus a stable `patch` for it. */
export function useTabUi(tabId: string): [TabUiState, (patch: Partial<TabUiState>) => void] {
  const state = useDataUiStore((s) => s.byTab[tabId]) ?? DEFAULT_UI;
  const patchAll = useDataUiStore((s) => s.patch);
  return [state, (patch) => patchAll(tabId, patch)];
}

// -- grid models --------------------------------------------------------------------------------

const models = new Map<string, GridModel>();
/** Scroll positions survive sub-tab switches without causing renders. */
const scrollPositions = new Map<string, { top: number; left: number }>();

export function getGridModel(tabId: string, sessionId: Uuid, table: ObjectPath): GridModel {
  let m = models.get(tabId);
  if (!m) {
    m = new GridModel({
      sessionId,
      table,
      // Row indexes mean other records after a reload: drop the selection.
      onReset: () => useDataUiStore.getState().patch(tabId, { selection: null }),
    });
    models.set(tabId, m);
  }
  return m;
}

export function peekGridModel(tabId: string): GridModel | undefined {
  return models.get(tabId);
}

export function saveScroll(tabId: string, top: number, left: number): void {
  scrollPositions.set(tabId, { top, left });
}

export function loadScroll(tabId: string): { top: number; left: number } {
  return scrollPositions.get(tabId) ?? { top: 0, left: 0 };
}

/** Release everything held for a tab (cancels its running fetch). */
export function disposeTabData(tabId: string): void {
  models.get(tabId)?.dispose();
  models.delete(tabId);
  scrollPositions.delete(tabId);
  useDataUiStore.getState().drop(tabId);
}

/** Test helper: forget all tabs. */
export function resetDataStores(): void {
  for (const id of [...models.keys()]) disposeTabData(id);
  useDataUiStore.setState({ byTab: {} });
}

// Clean up when a tab disappears from the tabs store.
let previousIds = new Set(useTabsStore.getState().tabs.map((t) => t.id));
useTabsStore.subscribe((state) => {
  const ids = new Set(state.tabs.map((t) => t.id));
  for (const id of previousIds) if (!ids.has(id)) disposeTabData(id);
  previousIds = ids;
});
