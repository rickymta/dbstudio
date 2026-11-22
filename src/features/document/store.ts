// Per-tab UI state of the document view (the tab registry renders only the active tab, so
// component state would be lost on every tab switch). Rows and filters live in the data feature's
// GridModel / tab store; this keeps the view mode and the selected document.

import { create } from 'zustand';
import type { Value } from '@/lib/types';
import { allTabs, useTabsStore } from '@/stores/tabs';
import { docId } from './flatten';

export type DocViewMode = 'table' | 'tree' | 'json';

export interface DocUiState {
  mode: DocViewMode;
  /** `_id` of the selected document as `idKey` text (row indexes change on reload). */
  selectedId: string | null;
}

const DEFAULT_UI: DocUiState = { mode: 'table', selectedId: null };

interface DocUiStore {
  byTab: Record<string, DocUiState>;
  patch: (tabId: string, patch: Partial<DocUiState>) => void;
  drop: (tabId: string) => void;
}

export const useDocUiStore = create<DocUiStore>((set) => ({
  byTab: {},
  patch: (tabId, patch) =>
    set((s) => ({
      byTab: { ...s.byTab, [tabId]: { ...(s.byTab[tabId] ?? DEFAULT_UI), ...patch } },
    })),
  drop: (tabId) =>
    set((s) => {
      if (!(tabId in s.byTab)) return s;
      const byTab = { ...s.byTab };
      delete byTab[tabId];
      return { byTab };
    }),
}));

export function useDocUi(tabId: string): [DocUiState, (patch: Partial<DocUiState>) => void] {
  const state = useDocUiStore((s) => s.byTab[tabId]) ?? DEFAULT_UI;
  const patchAll = useDocUiStore((s) => s.patch);
  return [state, (patch) => patchAll(tabId, patch)];
}

/** Stable text identity of a document's `_id` (`null` when it has none). */
export function idKey(doc: Value | undefined): string | null {
  const id = docId(doc);
  return id ? JSON.stringify(id) : null;
}

/** Test helper. */
export function resetDocStores(): void {
  useDocUiStore.setState({ byTab: {} });
}

// Tabs of a workspace that is not shown still exist: only a tab gone from every scope is closed.
let previousIds = new Set(allTabs(useTabsStore.getState()).map((t) => t.id));
useTabsStore.subscribe((state) => {
  const ids = new Set(allTabs(state).map((t) => t.id));
  for (const id of previousIds) if (!ids.has(id)) useDocUiStore.getState().drop(id);
  previousIds = ids;
});
