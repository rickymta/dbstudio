// Tier 2 of the Query Advisor on the UI side: asks the backend to match the advice against the
// catalog (`planAdvise`) and keeps the answer per plan next to the tier-1 advice of the tree.

import { useEffect, useMemo } from 'react';
import { create } from 'zustand';
import { ipc, toIpcError } from '@/lib/ipc';
import type { PlanAdvice, RawPlan } from '@/lib/types';
import { useQuery } from '@tanstack/react-query';
import { settingsQueryKey } from '@/features/settings/queries';
import { IDLE_STATUS, rankAdvice, statusOfReport, type AdviceStatus } from './adviceModel';
import { usePlanStore } from './planStore';

interface Slot {
  /** The raw plan the slot answers; a slot of an older run is ignored. */
  raw: RawPlan;
  /** Advice of the tier-2 report (`null` while pending or when the call failed / is off). */
  advice: PlanAdvice[] | null;
  status: AdviceStatus;
}

interface AdviceState {
  slots: Record<string, Slot>;
  set: (planId: string, slot: Slot) => void;
  remove: (planId: string) => void;
}

export const useAdviceStore = create<AdviceState>((set) => ({
  slots: {},
  set: (planId, slot) => set((s) => ({ slots: { ...s.slots, [planId]: slot } })),
  remove: (planId) =>
    set((s) => {
      if (!(planId in s.slots)) return s;
      const slots = { ...s.slots };
      delete slots[planId];
      return { slots };
    }),
}));

// Forget the answers of closed plans.
usePlanStore.subscribe((state) => {
  const { slots, remove } = useAdviceStore.getState();
  for (const id of Object.keys(slots)) if (!(id in state.entries)) remove(id);
});

export interface PlanAdviceView {
  /** Ranked by impact. */
  ranked: PlanAdvice[];
  status: AdviceStatus;
}

const EMPTY: PlanAdvice[] = [];

/**
 * Advice of a plan: the tier-2 answer when there is one for the current run, else the tier-1
 * advice carried by the tree. Read-only: it never calls the backend.
 */
export function usePlanAdvice(planId: string): PlanAdviceView {
  const raw = usePlanStore((s) => s.entries[planId]?.raw ?? null);
  const base = usePlanStore((s) => s.entries[planId]?.tree?.advice ?? EMPTY);
  const slot = useAdviceStore((s) => s.slots[planId]);
  const current = slot && slot.raw === raw ? slot : null;
  const advice = current?.advice ?? base;
  const ranked = useMemo(() => rankAdvice(advice), [advice]);
  return { ranked, status: current?.status ?? IDLE_STATUS };
}

/**
 * Run tier 2 for `planId` whenever its plan changes (call once, from the plan tab). Needs the
 * plan's session; without one, or with the setting off, the tier-1 advice stays.
 */
export function useAdviceRefresh(planId: string): void {
  // Wait for the setting: asking first and finding it switched off later would already have read
  // the catalog.
  const settings = useQuery({
    queryKey: settingsQueryKey,
    queryFn: ipc.settingsGet,
    staleTime: Infinity,
  });
  const ready = settings.isSuccess || settings.isError;
  const enabled = settings.data?.planAdvisorMetadata ?? true;
  const raw = usePlanStore((s) => s.entries[planId]?.raw ?? null);
  const hasTree = usePlanStore((s) => !!s.entries[planId]?.tree);
  useEffect(() => {
    if (!raw || !hasTree || !ready) return;
    const set = useAdviceStore.getState().set;
    if (!enabled) {
      set(planId, {
        raw,
        advice: null,
        status: {
          kind: 'disabled',
          metadata: null,
          error: null,
        },
      });
      return;
    }
    const entry = usePlanStore.getState().entries[planId];
    if (!entry) return;
    let alive = true;
    set(planId, { raw, advice: null, status: { kind: 'checking', metadata: null, error: null } });
    ipc
      .planAdvise(
        entry.sessionId,
        entry.leaseId,
        raw,
        entry.request?.database ?? null,
        null,
        entry.catalog !== false,
      )
      .then((report) => {
        if (!alive) return;
        set(planId, { raw, advice: report.advice, status: statusOfReport(report.metadata) });
      })
      .catch((e: unknown) => {
        if (!alive) return;
        set(planId, {
          raw,
          advice: null,
          status: { kind: 'failed', metadata: null, error: toIpcError(e).message },
        });
      });
    return () => {
      alive = false;
    };
  }, [planId, raw, hasTree, enabled, ready]);
}
