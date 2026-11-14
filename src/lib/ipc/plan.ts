import { mock } from '../mock';
import type {
  ConfirmationGrant,
  ExplainRequest,
  PlanHistoryEntry,
  PlanHistoryRecord,
  PlanResult,
  PlanRules,
  PlanTree,
  RawPlan,
  Uuid,
} from '../types';
import { call } from './core';

/** Query plans (EXPLAIN). */
export const planApi = {
  planExplain: (
    sessionId: Uuid,
    leaseId: Uuid | null,
    request: ExplainRequest,
    rules?: PlanRules | null,
    /** Answer to a `confirmationRequired` error (ANALYZE of a write on a production connection). */
    confirm?: ConfirmationGrant | null,
  ): Promise<PlanResult> =>
    call('plan_explain', () => mock.planExplain(sessionId, leaseId, request, rules ?? null), {
      sessionId,
      leaseId,
      request,
      rules: rules ?? null,
      confirm: confirm ?? null,
    }),
  planParse: (raw: RawPlan, rules?: PlanRules | null): Promise<PlanTree> =>
    call('plan_parse', () => mock.planParse(raw, rules ?? null), { raw, rules: rules ?? null }),
  /** Save a `.dbplan.json`; resolves to the path (`null` = cancelled). */
  planFileSave: (raw: RawPlan): Promise<string | null> =>
    call('plan_file_save', () => mock.planFileSave(raw), { raw }),
  planFileOpen: (): Promise<RawPlan | null> => call('plan_file_open', () => mock.planFileOpen()),
  /** Newest first; `before` = `createdAt` of the last entry of the previous page. */
  planHistoryList: (
    connectionId: Uuid | null,
    limit: number,
    before?: string | null,
  ): Promise<PlanHistoryEntry[]> =>
    call('plan_history_list', () => mock.planHistoryList(connectionId, limit, before ?? null), {
      connectionId,
      limit,
      before: before ?? null,
    }),
  planHistoryGet: (id: Uuid): Promise<PlanHistoryRecord> =>
    call('plan_history_get', () => mock.planHistoryGet(id), { id }),
  planHistoryDelete: (id: Uuid): Promise<void> =>
    call('plan_history_delete', () => mock.planHistoryDelete(id), { id }),
  /** Resolves to the number of deleted entries (`null` = every connection). */
  planHistoryClear: (connectionId: Uuid | null): Promise<number> =>
    call('plan_history_clear', () => mock.planHistoryClear(connectionId), { connectionId }),
};
