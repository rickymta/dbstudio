// Query plans (dbstudio-core `plan.rs`, dbstudio-plan).

import type { ObjectPath, Uuid } from './core';
import type { TextRange } from './script';

export type PlanFormat =
  | 'postgresJson'
  | 'mySqlJson'
  | 'mySqlJsonV2'
  | 'mySqlTree'
  | 'mariaDbJson'
  | 'msSqlShowplanXml'
  | 'oraclePlanRows'
  | 'mongoExplain'
  | 'clickHouseJson'
  | 'cockroachText'
  | 'tiDbRows'
  | 'cqlTrace';

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

export type PredicateRole =
  'indexCond' | 'filter' | 'joinCond' | 'joinFilter' | 'recheck' | 'sortKey' | 'groupKey' | 'other';

/** One condition of a node with its role (filled by the parsers that know them). */
export interface NodePredicate {
  role: PredicateRole;
  text: string;
}

export interface PlanNode {
  /** Pre-order index, root = 0. */
  id: number;
  op: string;
  kind: PlanNodeKind;
  object: PlanObject | null;
  condition: string | null;
  /** Absent in plans from older builds and for formats whose parser does not fill it. */
  predicates?: NodePredicate[];
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
  /** Query Advisor output, ranked. Absent in old mocks: always read `?? []`. */
  advice?: PlanAdvice[];
}

export interface PlanRules {
  largeTableRows?: number;
  misestimateFactor?: number;
  misestimateMinRows?: number;
  nestedLoopRows?: number;
  /** Advisor thresholds. */
  filterMinExamined?: number;
  filterMaxSelectivity?: number;
  residualRatio?: number;
  heapFetchRatio?: number;
  largeOffset?: number;
  minShare?: number;
  maxAdvice?: number;
}

// ---- Query Advisor (dbstudio-plan `advice`) ----

export type AdviceRule =
  | 'indexForFilter'
  | 'indexResidualFilter'
  | 'indexForSort'
  | 'nestedLoopInnerScan'
  | 'staleStatistics'
  | 'sortSpill'
  | 'hashSpill'
  | 'fullScanReview'
  | 'existingIndexUnused'
  | 'pgBitmapLossy'
  | 'pgHeapFetches'
  | 'pgFunctionOnColumn'
  | 'implicitConversion'
  | 'pgParallelNotLaunched'
  | 'pgJitOverhead'
  | 'pgColdCache'
  | 'pgNotInSubplan'
  | 'mysqlPossibleKeysUnused'
  | 'mysqlTempTableGroup'
  | 'mysqlJoinBuffer'
  | 'mysqlIndexMerge'
  | 'mssqlMissingIndex'
  | 'mssqlKeyLookup'
  | 'mssqlNoJoinPredicate'
  | 'mssqlNoStatistics'
  | 'mssqlExcessiveGrant'
  | 'oracleCartesian'
  | 'oracleIndexSkipScan'
  | 'mongoCollscan'
  | 'mongoExaminedRatio'
  | 'mongoBlockingSort'
  | 'mongoLookupNoIndex'
  | 'mongoUnanchoredRegex'
  | 'chPrimaryKeyUnused'
  | 'chPartitionNotPruned'
  | 'chSkipIndexUseless'
  | 'chFinal'
  | 'chJoinRightLarge'
  | 'crdbIndexRecommendation'
  | 'crdbMissingStats'
  | 'tidbPseudoStats'
  | 'nonSargablePredicate'
  | 'leadingWildcardLike'
  | 'orAcrossColumns'
  | 'selectStar'
  | 'largeOffset'
  | 'notInSubquery'
  | 'typeMismatch'
  /** A rule newer than this build. */
  | 'unknown';

/** Every rule name, for exhaustive checks (i18n coverage tests). */
export const ADVICE_RULES: readonly AdviceRule[] = [
  'indexForFilter',
  'indexResidualFilter',
  'indexForSort',
  'nestedLoopInnerScan',
  'staleStatistics',
  'sortSpill',
  'hashSpill',
  'fullScanReview',
  'existingIndexUnused',
  'pgBitmapLossy',
  'pgHeapFetches',
  'pgFunctionOnColumn',
  'implicitConversion',
  'pgParallelNotLaunched',
  'pgJitOverhead',
  'pgColdCache',
  'pgNotInSubplan',
  'mysqlPossibleKeysUnused',
  'mysqlTempTableGroup',
  'mysqlJoinBuffer',
  'mysqlIndexMerge',
  'mssqlMissingIndex',
  'mssqlKeyLookup',
  'mssqlNoJoinPredicate',
  'mssqlNoStatistics',
  'mssqlExcessiveGrant',
  'oracleCartesian',
  'oracleIndexSkipScan',
  'mongoCollscan',
  'mongoExaminedRatio',
  'mongoBlockingSort',
  'mongoLookupNoIndex',
  'mongoUnanchoredRegex',
  'chPrimaryKeyUnused',
  'chPartitionNotPruned',
  'chSkipIndexUseless',
  'chFinal',
  'chJoinRightLarge',
  'crdbIndexRecommendation',
  'crdbMissingStats',
  'tidbPseudoStats',
  'nonSargablePredicate',
  'leadingWildcardLike',
  'orAcrossColumns',
  'selectStar',
  'largeOffset',
  'notInSubquery',
  'typeMismatch',
];

export type Confidence = 'high' | 'medium' | 'low';

export interface TableRef {
  database?: string | null;
  schema?: string | null;
  name: string;
  alias?: string | null;
  /** Resolved by `planAdvise`; enables "Open table". */
  path?: ObjectPath | null;
}

export type EvidenceUnit = 'rows' | 'ms' | 'bytes' | 'blocks' | 'pct' | 'ratio' | 'count' | 'kb';

export interface Evidence {
  /** i18n `advice.evidence.<key>`. */
  key: string;
  nodeId?: number | null;
  value?: number | null;
  text?: string | null;
  unit?: EvidenceUnit | null;
  /** true = measured (ANALYZE), false = estimate. */
  actual: boolean;
}

export type ActionKind =
  | 'createIndex'
  | 'statistics'
  | 'sessionSetting'
  | 'maintenance'
  | 'rewrite'
  | 'hint'
  | 'check'
  | 'serverSetting';
export type ActionScope = 'query' | 'session' | 'object' | 'database' | 'server';
export type LockRisk =
  'none' | 'shortLock' | 'blocksWrites' | 'blocksReadsWrites' | 'heavyMutation';

export interface AdviceAction {
  kind: ActionKind;
  /** i18n `advice.actions.<labelKey>`. */
  labelKey: string;
  /** Ready text in the connection's language (SQL, mongo shell); absent for descriptions. */
  sql?: string | null;
  /** Has placeholders: never "ready to run". */
  template: boolean;
  scope: ActionScope;
  lock?: LockRisk | null;
  params?: Record<string, string>;
}

export interface Caveat {
  /** i18n `advice.caveats.<key>`. */
  key: string;
  /** The user must verify before acting. */
  verify: boolean;
  params?: Record<string, string>;
}

export type ImpactBasis = 'time' | 'cost' | 'rows' | 'none';

export interface Impact {
  basis: ImpactBasis;
  /** Share of the whole statement attributed to the nodes (0..1); null = unknown. */
  share?: number | null;
  score: number;
}

export interface PlanAdvice {
  /** Stable per plan: `{rule}:{hash8}`. */
  id: string;
  rule: AdviceRule;
  severity: Severity;
  confidence: Confidence;
  /** Nodes concerned, most relevant first (empty for statement-only advice). */
  nodeIds: number[];
  table?: TableRef | null;
  /** Values for `advice.rules.<rule>.title` / `.why`. */
  params: Record<string, string>;
  evidence: Evidence[];
  actions: AdviceAction[];
  caveats: Caveat[];
  impact: Impact;
  fromWarnings: PlanRule[];
  /** Character range in `RawPlan.statement` for statement rules. */
  statementRange?: TextRange | null;
  /** Plan-independent identity for compare. */
  fingerprint: string;
}

export type MetadataState = 'notRequested' | 'checked' | 'partial' | 'unavailable' | 'disabled';

export interface TableFailure {
  table: string;
  reason: 'timeout' | 'notFound' | 'ambiguous' | 'unsupported' | 'error';
  message: string;
}

export interface AdviceMetadata {
  state: MetadataState;
  checked: TableRef[];
  failed: TableFailure[];
  skipped: number;
  elapsedMs: number;
  /** Why the catalog was not read although it was wanted. */
  reason?: 'planWithoutConnection' | 'dialectMismatch' | null;
}

export interface AdviceReport {
  advice: PlanAdvice[];
  metadata: AdviceMetadata;
}

export interface PlanResult {
  raw: RawPlan;
  tree: PlanTree | null;
  parseError: string | null;
}
