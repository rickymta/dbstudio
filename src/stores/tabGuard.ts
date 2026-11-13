import { useEffect, useRef } from 'react';

/** Asked before a tab closes; resolve `false` to keep it open (e.g. an open transaction). */
export type TabGuard = () => boolean | Promise<boolean>;

// Guards are functions, so they live next to the store instead of inside it.
const guards = new Map<string, TabGuard>();

/** Register `guard` for `tabId`; the returned function removes it. */
export function registerTabGuard(tabId: string, guard: TabGuard): () => void {
  guards.set(tabId, guard);
  return () => {
    if (guards.get(tabId) === guard) guards.delete(tabId);
  };
}

/** The guard of a tab, if one is registered. */
export function getTabGuard(tabId: string): TabGuard | undefined {
  return guards.get(tabId);
}

/**
 * Let a tab veto closing. `guard` may change on every render; the latest one is used.
 * The tab bar and `requestClose` consult it.
 */
export function useTabGuard(tabId: string, guard: TabGuard): void {
  const latest = useRef(guard);
  useEffect(() => {
    latest.current = guard;
  });
  useEffect(() => registerTabGuard(tabId, () => latest.current()), [tabId]);
}
