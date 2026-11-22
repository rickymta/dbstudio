import { create } from 'zustand';
import { ipc, toIpcError } from '@/lib/ipc';
import { guardedCall } from '@/features/safety/useGuardedCall';
import type { ExplainRequest, IpcError, PlanTree, RawPlan, Uuid } from '@/lib/types';
import { allTabs, planTabId, useTabsStore } from '@/stores/tabs';

export type PlanStatus = 'loading' | 'ready' | 'error';

/** One plan shown in a `plan` tab. */
export interface PlanEntry {
  planId: string;
  sessionId: Uuid;
  connectionId: Uuid;
  /** Dedicated connection the statement was explained on (editor tab lease), if any. */
  leaseId: Uuid | null;
  /** Editor tab (`tabStateId`) the plan was explained from: "Insert into editor" appends there. */
  sourceEditorId?: string | null;
  /**
   * The plan comes from this connection (explained in the tab). `false` for a plan opened from
   * a file: its tables are not looked up in the catalog of the session that happens to be open.
   */
  catalog?: boolean;
  /** What was asked; `null` for a plan opened from a file. */
  request: ExplainRequest | null;
  /** The statement text (also set for plans opened from a file). */
  statement: string;
  raw: RawPlan | null;
  tree: PlanTree | null;
  /** The raw plan arrived but could not be parsed. */
  parseError: string | null;
  status: PlanStatus;
  error: IpcError | null;
  /** Node selected in the diagram, table and raw views alike. */
  selectedId: number | null;
  /** Incremented per run; a late answer of an older run is ignored. */
  runId: number;
}

interface PlanState {
  entries: Record<string, PlanEntry>;
  add: (entry: PlanEntry) => void;
  patch: (planId: string, patch: Partial<PlanEntry>) => void;
  select: (planId: string, nodeId: number | null) => void;
  remove: (planId: string) => void;
}

export const usePlanStore = create<PlanState>((set) => ({
  entries: {},
  add: (entry) => set((s) => ({ entries: { ...s.entries, [entry.planId]: entry } })),
  patch: (planId, patch) =>
    set((s) => {
      const cur = s.entries[planId];
      return cur ? { entries: { ...s.entries, [planId]: { ...cur, ...patch } } } : s;
    }),
  select: (planId, nodeId) =>
    set((s) => {
      const cur = s.entries[planId];
      return cur && cur.selectedId !== nodeId
        ? { entries: { ...s.entries, [planId]: { ...cur, selectedId: nodeId } } }
        : s;
    }),
  remove: (planId) =>
    set((s) => {
      if (!(planId in s.entries)) return s;
      const entries = { ...s.entries };
      delete entries[planId];
      return { entries };
    }),
}));

/** Stop the explain that is running for `planId` (no effect when nothing runs). */
export function cancelPlan(planId: string): void {
  const entry = usePlanStore.getState().entries[planId];
  if (entry?.status !== 'loading' || !entry.request) return;
  void ipc.queryCancel(entry.sessionId, entry.request.queryId).catch(() => {});
}

// Forget the plans of closed tabs; a tab closed while its statement runs stops the statement.
useTabsStore.subscribe((state) => {
  const open = new Set(allTabs(state).map((t) => t.id));
  const { entries, remove } = usePlanStore.getState();
  for (const id of Object.keys(entries)) {
    if (!open.has(planTabId(id))) {
      cancelPlan(id);
      remove(id);
    }
  }
});

/**
 * Explain `entry.request` and store the answer. `analyze` overrides the request's flag (the
 * request is updated, so "run again" repeats what was last asked).
 */
export async function runPlan(planId: string, opts: { analyze?: boolean } = {}): Promise<void> {
  const store = usePlanStore.getState();
  const entry = store.entries[planId];
  if (!entry?.request) return;
  const request: ExplainRequest = {
    ...entry.request,
    queryId: crypto.randomUUID(),
    analyze: opts.analyze ?? entry.request.analyze ?? false,
  };
  const runId = entry.runId + 1;
  store.patch(planId, {
    request,
    runId,
    status: 'loading',
    error: null,
    parseError: null,
  });
  try {
    // A production ANALYZE of a write asks for the typed confirmation (`ConfirmDestructiveHost`).
    const explain = (leaseId: string | null) =>
      guardedCall((confirm) =>
        ipc.planExplain(entry.sessionId, leaseId, request, null, confirm ?? null),
      );
    let result;
    try {
      result = await explain(entry.leaseId);
    } catch (e) {
      // A driver without EXPLAIN on its dedicated connection: explain through the session.
      const err = toIpcError(e);
      if (entry.leaseId && err.kind === 'unsupported' && err.capability === 'explain') {
        result = await explain(null);
      } else {
        throw e;
      }
    }
    const cur = usePlanStore.getState().entries[planId];
    if (!cur || cur.runId !== runId) return;
    usePlanStore.getState().patch(planId, {
      raw: result.raw,
      tree: result.tree,
      parseError: result.parseError,
      status: 'ready',
      selectedId: null,
    });
  } catch (e) {
    const cur = usePlanStore.getState().entries[planId];
    if (!cur || cur.runId !== runId) return;
    const error = toIpcError(e);
    // A cancelled (or declined) run leaves the previous plan (if any) on screen.
    const keep = cur.raw !== null && error.kind === 'cancelled';
    usePlanStore.getState().patch(planId, { status: keep ? 'ready' : 'error', error });
  }
}

/** Parse a raw plan (opened from a file) and store the result. */
export async function loadRawPlan(planId: string, raw: RawPlan): Promise<void> {
  const store = usePlanStore.getState();
  const entry = store.entries[planId];
  if (!entry) return;
  const runId = entry.runId + 1;
  store.patch(planId, { raw, runId, status: 'loading', error: null, parseError: null });
  try {
    const tree = await ipc.planParse(raw);
    const cur = usePlanStore.getState().entries[planId];
    if (!cur || cur.runId !== runId) return;
    usePlanStore.getState().patch(planId, { tree, status: 'ready', selectedId: null });
  } catch (e) {
    const cur = usePlanStore.getState().entries[planId];
    if (!cur || cur.runId !== runId) return;
    const err = toIpcError(e);
    usePlanStore.getState().patch(planId, { status: 'ready', tree: null, parseError: err.message });
  }
}
