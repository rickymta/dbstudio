// Query plans (dbstudio-core `plan.rs`, dbstudio-plan).

import type { Uuid } from './core';

export type PlanFormat =
  | 'postgresJson'
  | 'mySqlJson'
  | 'mySqlJsonV2'
  | 'mySqlTree'
  | 'mariaDbJson'
  | 'msSqlShowplanXml'
  | 'oraclePlanRows'
  | 'mongoExplain';

export interface ExplainOptions {
  buffers?: boolean;
  verbose?: boolean;
  settings?: boolean;
}

export interface ExplainRequest {
  queryId: Uuid;
  /** Exactly one statement. */
  text: string;
  database?: string | null;
  analyze?: boolean;
  options?: ExplainOptions;
  timeoutMs?: number | null;
}

export interface RawPlan {
  format: PlanFormat;
  /** Unparsed source exactly as received. */
  text: string;
  statement: string;
  analyzed: boolean;
  /** ANALYZE ran inside a transaction that was rolled back. */
  rolledBack: boolean;
  serverVersion: string;
  /** RFC 3339 UTC. */
  capturedAt: string;
  extra?: Record<string, string>;
}

export type PlanNodeKind =
  | 'fullScan'
  | 'indexScan'
  | 'indexOnlyScan'
  | 'indexSeek'
  | 'keyLookup'
  | 'nestedLoopJoin'
  | 'hashJoin'
  | 'mergeJoin'
  | 'sort'
  | 'aggregate'
  | 'hashAggregate'
  | 'window'
  | 'limit'
  | 'filter'
  | 'materialize'
  | 'subquery'
  | 'union'
  | 'gather'
  | 'modify'
  | 'fetch'
  | 'result'
  | 'other';

export interface PlanObject {
  schema: string | null;
  name: string;
  alias: string | null;
  index: string | null;
}

export type RawRef =
  | { kind: 'jsonPointer'; pointer: string }
  | { kind: 'xmlNode'; nodeId: string }
  | { kind: 'lines'; start: number; end: number }
  | { kind: 'row'; id: number }
  | { kind: 'none' };

export interface PlanNode {
  /** Pre-order index, root = 0. */
  id: number;
  op: string;
  kind: PlanNodeKind;
  object: PlanObject | null;
  condition: string | null;
  estStartupCost: number | null;
  estCost: number | null;
  estRows: number | null;
  actualRows: number | null;
  actualRowsTotal: number | null;
  actualTimeMs: number | null;
  loops: number | null;
  selfCost: number | null;
  selfTimeMs: number | null;
  children: PlanNode[];
  extra: Record<string, string>;
  raw: RawRef;
}

export interface PlanTotals {
  totalCost: number | null;
  planningTimeMs: number | null;
  executionTimeMs: number | null;
  rows: number | null;
}

export type PlanRule = 'fullScanLargeTable' | 'misestimate' | 'spillToDisk' | 'nestedLoopManyRows';
export type Severity = 'info' | 'warning' | 'critical';

export interface PlanWarning {
  nodeId: number;
  rule: PlanRule;
  severity: Severity;
  params: Record<string, string>;
}

export interface PlanTree {
  format: PlanFormat;
  analyzed: boolean;
  root: PlanNode;
  totals: PlanTotals;
  warnings: PlanWarning[];
  /** Node ids from the root following the maximum inclusive time (else cost). */
  hotPath: number[];
  nodeCount: number;
}

export interface PlanRules {
  largeTableRows?: number;
  misestimateFactor?: number;
  misestimateMinRows?: number;
  nestedLoopRows?: number;
}

export interface PlanResult {
  raw: RawPlan;
  tree: PlanTree | null;
  parseError: string | null;
}
