// Registry through which the SQL debugger saves and restores its per-tab state (breakpoints,
// watches) with the editor tabs without `persist.ts` knowing about it. Defaults do nothing.

import type { TabDebugState, Uuid } from '@/lib/types';

interface PersistHooks {
  /** Debugger state of a tab when the tabs are collected for saving (`null` = nothing). */
  debugStateOf: (tabId: Uuid) => TabDebugState | null;
  /** Called when a tab is restored with saved debugger state. */
  applyDebugState: (tabId: Uuid, state: TabDebugState) => void;
}

const hooks: PersistHooks = { debugStateOf: () => null, applyDebugState: () => {} };
const listeners = new Set<() => void>();

/** The debugger registers its implementations here (once, at load). */
export function registerPersistHooks(next: Partial<PersistHooks>): void {
  Object.assign(hooks, next);
}

export const debugStateOf = (tabId: Uuid): TabDebugState | null => hooks.debugStateOf(tabId);
export const applyDebugState = (tabId: Uuid, state: TabDebugState): void =>
  hooks.applyDebugState(tabId, state);

/** Subscribe to "debugger state changed, save the tabs"; returns the unsubscribe function. */
export function onDebugStateChanged(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** The debugger calls this after it changed breakpoints or watches. */
export function notifyDebugStateChanged(): void {
  for (const listener of [...listeners]) listener();
}
