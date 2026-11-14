// Transient UI state of the debugger: which dialog is open for a tab and the breakpoint popover.

import { create } from 'zustand';

export type DebugDialog =
  | { kind: 'breakpoints' }
  | { kind: 'evaluate'; expr?: string }
  /** Commit / Rollback / Keep question; `then` is what follows the answer. */
  | { kind: 'end'; then: 'stop' | 'restart' }
  | { kind: 'smartStepInto' }
  | { kind: 'routineSupport' }
  | { kind: 'inspect'; name: string; text: string };

export interface BreakpointPopover {
  scopeKey: string;
  breakpointId: string;
  /** Viewport position of the right click. */
  x: number;
  y: number;
}

interface DebugUiState {
  dialogs: Record<string, DebugDialog | null>;
  popover: BreakpointPopover | null;
  openDialog: (tabId: string, dialog: DebugDialog) => void;
  closeDialog: (tabId: string) => void;
  openPopover: (popover: BreakpointPopover) => void;
  closePopover: () => void;
}

export const useDebugUi = create<DebugUiState>((set) => ({
  dialogs: {},
  popover: null,
  openDialog: (tabId, dialog) => set((s) => ({ dialogs: { ...s.dialogs, [tabId]: dialog } })),
  closeDialog: (tabId) => set((s) => ({ dialogs: { ...s.dialogs, [tabId]: null } })),
  openPopover: (popover) => set({ popover }),
  closePopover: () => set({ popover: null }),
}));
