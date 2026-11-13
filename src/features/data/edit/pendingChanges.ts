import { create } from 'zustand';
import type { RowChange } from '@/lib/types';

interface PendingChangesState {
  /** Uncommitted row changes per data tab id. */
  byTab: Record<string, RowChange[]>;
  add: (tabId: string, change: RowChange) => void;
  clear: (tabId: string) => void;
}

export const usePendingChanges = create<PendingChangesState>((set) => ({
  byTab: {},
  add: (tabId, change) =>
    set((s) => ({ byTab: { ...s.byTab, [tabId]: [...(s.byTab[tabId] ?? []), change] } })),
  clear: (tabId) =>
    set((s) => {
      const byTab = { ...s.byTab };
      delete byTab[tabId];
      return { byTab };
    }),
}));
