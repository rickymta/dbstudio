// Mock query plans: unavailable in the browser mock.

import type { ExplainRequest, PlanResult, PlanRules, PlanTree, RawPlan, Uuid } from '../types';
import { mockSessionKind } from './sessions';
import { unsupported } from './util';

export const planMock = {
  async planExplain(
    sessionId: Uuid,
    leaseId: Uuid | null,
    request: ExplainRequest,
    rules: PlanRules | null,
  ): Promise<PlanResult> {
    mockSessionKind(sessionId);
    void [leaseId, request, rules];
    throw unsupported('explain');
  },
  async planParse(raw: RawPlan, rules: PlanRules | null): Promise<PlanTree> {
    void [raw, rules];
    throw unsupported('explain');
  },
  async planFileSave(raw: RawPlan): Promise<string | null> {
    void raw;
    return null;
  },
  async planFileOpen(): Promise<RawPlan | null> {
    return null;
  },
};
