// Per-tab edit session: the table's editability, whether edit mode is on and which cell is being
// edited. Dropped together with the tab's other data state.

import { create } from 'zustand';
import type { Editability, IpcError } from '@/lib/types';

export interface EditingCell {
  /** Grid row index (rows past the loaded ones are pending inserts). */
  row: number;
  /** Real column index. */
  col: number;
}

export interface EditSessionState {
  editability: Editability | null;
  /** Reading the editability failed. */
  error: IpcError | null;
  editMode: boolean;
  editing: EditingCell | null;
  /** The last apply failed at this change (index into the change list) and was rolled back. */
  failure: { index: number; message: string } | null;
  /** An apply is in flight: the grid is locked so no edit can slip between send and clear. */
  applying: boolean;
}

export const INITIAL_EDIT_SESSION: EditSessionState = {
  editability: null,
  error: null,
  editMode: false,
  editing: null,
  failure: null,
  applying: false,
};

interface EditSessionStore {
  byTab: Record<string, EditSessionState>;
  patch: (tabId: string, patch: Partial<EditSessionState>) => void;
  drop: (tabId: string) => void;
}

export const useEditSessionStore = create<EditSessionStore>((set) => ({
  byTab: {},
  patch: (tabId, patch) =>
    set((s) => ({
      byTab: {
        ...s.byTab,
        [tabId]: { ...(s.byTab[tabId] ?? INITIAL_EDIT_SESSION), ...patch },
      },
    })),
  drop: (tabId) =>
    set((s) => {
      if (!(tabId in s.byTab)) return s;
      const byTab = { ...s.byTab };
      delete byTab[tabId];
      return { byTab };
    }),
}));

export function useEditSession(tabId: string): EditSessionState {
  return useEditSessionStore((s) => s.byTab[tabId]) ?? INITIAL_EDIT_SESSION;
}

export function patchEditSession(tabId: string, patch: Partial<EditSessionState>): void {
  useEditSessionStore.getState().patch(tabId, patch);
}
