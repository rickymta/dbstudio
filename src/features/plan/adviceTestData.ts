// Shared fixtures of the advisor tests (not imported by app code).
import type { PlanAdvice } from '@/lib/types';

export function makeAdvice(over: Partial<PlanAdvice> & { id: string }): PlanAdvice {
  return {
    rule: 'indexForFilter',
    severity: 'warning',
    confidence: 'high',
    nodeIds: [3],
    table: { name: 'orders', schema: 'public', path: null },
    params: { table: 'orders', columns: 'status' },
    evidence: [],
    actions: [],
    caveats: [],
    impact: { basis: 'time', share: 0.5, score: 0.5 },
    fromWarnings: [],
    fingerprint: over.id,
    ...over,
  };
}
