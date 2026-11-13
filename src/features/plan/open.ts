import { ipc, toIpcError } from '@/lib/ipc';
import type { RawPlan, Uuid } from '@/lib/types';
import { toast } from '@/components/ui/toastStore';
import { planTabId, useTabsStore } from '@/stores/tabs';
import { loadRawPlan, runPlan, usePlanStore, type PlanEntry } from './planStore';

/** What a plan tab needs to know about where it runs (a `SessionInfo` fits). */
export interface PlanSession {
  sessionId: Uuid;
  connectionId: Uuid;
}

export interface OpenPlanOptions {
  session: PlanSession;
  /** Exactly one statement. */
  text: string;
  database?: string | null;
  /** Start with `ANALYZE` (the caller is responsible for warning before write statements). */
  analyze?: boolean;
  /** Dedicated connection of the editor tab, so session settings and temp tables apply. */
  leaseId?: Uuid | null;
  timeoutMs?: number | null;
}

function newEntry(session: PlanSession, planId: string, patch: Partial<PlanEntry>): PlanEntry {
  return {
    planId,
    sessionId: session.sessionId,
    connectionId: session.connectionId,
    leaseId: null,
    request: null,
    statement: '',
    raw: null,
    tree: null,
    parseError: null,
    status: 'loading',
    error: null,
    confirmation: null,
    selectedId: null,
    runId: 0,
    ...patch,
  };
}

function openTab(session: PlanSession, planId: string) {
  useTabsStore.getState().openTab({
    id: planTabId(planId),
    kind: 'plan',
    title: { key: 'tab.title', ns: 'plan' },
    closable: true,
    payload: { sessionId: session.sessionId, connectionId: session.connectionId, planId },
  });
}

/**
 * Explain a statement in a new plan tab and return the plan id. The tab opens immediately
 * (showing progress); the answer is stored when it arrives.
 */
export function openPlan(opts: OpenPlanOptions): string {
  const planId = crypto.randomUUID();
  usePlanStore.getState().add(
    newEntry(opts.session, planId, {
      leaseId: opts.leaseId ?? null,
      statement: opts.text,
      request: {
        queryId: crypto.randomUUID(),
        text: opts.text,
        database: opts.database ?? null,
        analyze: opts.analyze ?? false,
        options: {},
        timeoutMs: opts.timeoutMs ?? null,
      },
    }),
  );
  openTab(opts.session, planId);
  void runPlan(planId);
  return planId;
}

/** Open a plan from raw text (e.g. a `.dbplan.json` already read). */
export function openRawPlan(session: PlanSession, raw: RawPlan): string {
  const planId = crypto.randomUUID();
  usePlanStore.getState().add(newEntry(session, planId, { statement: raw.statement, raw }));
  openTab(session, planId);
  void loadRawPlan(planId, raw);
  return planId;
}

/** Ask for a `.dbplan.json` and open it; resolves to the plan id (`null` = cancelled). */
export async function openPlanFile(session: PlanSession): Promise<string | null> {
  try {
    const raw = await ipc.planFileOpen();
    return raw ? openRawPlan(session, raw) : null;
  } catch (e) {
    toast.error(toIpcError(e).message);
    return null;
  }
}
