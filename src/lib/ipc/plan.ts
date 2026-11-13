import { mock } from '../mock';
import type {
  ConfirmationGrant,
  ExplainRequest,
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
};
