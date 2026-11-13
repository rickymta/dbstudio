// Comparison of two plans (before / after a change of query or index).

import type { PlanNode, PlanTree } from '@/lib/types';
import { flatten, metricOf, objectLabel } from './planModel';

export interface TotalDiff {
  key: 'totalCost' | 'planningTimeMs' | 'executionTimeMs' | 'rows' | 'nodeCount' | 'warnings';
  a: number | null;
  b: number | null;
  /** `b - a` (`null` when one side is unknown). */
  delta: number | null;
  /** `b / a` (`null` when not defined). */
  ratio: number | null;
}

export interface NodeDiff {
  /** `op` plus object: how nodes are matched between the two plans. */
  signature: string;
  a: PlanNode | null;
  b: PlanNode | null;
  /** Difference of the node's own time (analyzed plans) or cost, `b - a`. */
  delta: number | null;
}

export interface PlanComparison {
  totals: TotalDiff[];
  nodes: NodeDiff[];
  /** What `NodeDiff.delta` measures. */
  metric: 'time' | 'cost' | null;
}

const signatureOf = (n: PlanNode) => `${n.op}|${objectLabel(n)}`;

function diff(key: TotalDiff['key'], a: number | null, b: number | null): TotalDiff {
  const both = a != null && b != null;
  return {
    key,
    a,
    b,
    delta: both ? b - a : null,
    ratio: both && a !== 0 ? b / a : null,
  };
}

function weight(n: PlanNode | null, metric: 'time' | 'cost' | null): number {
  if (!n) return 0;
  return (metric === 'time' ? n.selfTimeMs : n.selfCost) ?? 0;
}

/**
 * Match nodes by operator and object in pre-order (the k-th "Seq Scan orders" of A pairs with
 * the k-th of B), then list pairs and unmatched nodes, the biggest change first.
 */
export function comparePlans(a: PlanTree, b: PlanTree): PlanComparison {
  const metricA = metricOf(a);
  const metricB = metricOf(b);
  const metric =
    metricA === 'time' && metricB === 'time' ? 'time' : metricA && metricB ? 'cost' : null;
  const pool = new Map<string, PlanNode[]>();
  for (const n of flatten(b.root)) {
    const list = pool.get(signatureOf(n)) ?? [];
    list.push(n);
    pool.set(signatureOf(n), list);
  }
  const nodes: NodeDiff[] = [];
  for (const n of flatten(a.root)) {
    const m = pool.get(signatureOf(n))?.shift() ?? null;
    nodes.push({
      signature: signatureOf(n),
      a: n,
      b: m,
      delta: m && metric ? weight(m, metric) - weight(n, metric) : null,
    });
  }
  for (const rest of pool.values()) {
    for (const m of rest) {
      nodes.push({ signature: signatureOf(m), a: null, b: m, delta: null });
    }
  }
  const size = (d: NodeDiff) =>
    d.a && d.b ? Math.abs(d.delta ?? 0) : weight(d.a ?? d.b, metric) || Number.MAX_SAFE_INTEGER / 2;
  nodes.sort((x, y) => size(y) - size(x));
  return {
    metric,
    nodes,
    totals: [
      diff('totalCost', a.totals.totalCost, b.totals.totalCost),
      diff('planningTimeMs', a.totals.planningTimeMs, b.totals.planningTimeMs),
      diff('executionTimeMs', a.totals.executionTimeMs, b.totals.executionTimeMs),
      diff('rows', a.totals.rows, b.totals.rows),
      diff('nodeCount', a.nodeCount, b.nodeCount),
      diff('warnings', a.warnings.length, b.warnings.length),
    ],
  };
}
