// Sample Query Advisor output for the captured plans of `plan.ts`. The real engine
// (crates/dbstudio-plan `advice`) produces this shape; the browser mock has no engine, so these
// are hand-written but cover the variety the UI must render: every confidence level, a
// "needs verification" caveat, a template action, every scope, a statement range and
// advice without any node.

import type { AdviceAction, AdviceReport, PlanAdvice } from '../types';

const createIndex = (sql: string, index: string, table: string): AdviceAction => ({
  kind: 'createIndex',
  labelKey: 'createIndex',
  sql,
  template: false,
  scope: 'object',
  lock: 'shortLock',
  params: { index, table },
});

const analyzeAction = (sql: string, table: string): AdviceAction => ({
  kind: 'statistics',
  labelKey: 'refreshStatistics',
  sql,
  template: false,
  scope: 'object',
  lock: 'none',
  params: { table },
});

const PG_ORDERS = { database: null, schema: 'public', name: 'orders', alias: 'o', path: null };

/** PostgreSQL ANALYZE plan `pg_02`: a seq scan that keeps 97% of the rows is NOT advised. */
const pgActual: PlanAdvice[] = [
  {
    id: 'indexForFilter:3f9a1c20',
    rule: 'indexForFilter',
    severity: 'warning',
    confidence: 'high',
    nodeIds: [3],
    table: PG_ORDERS,
    params: {
      table: 'public.orders',
      columns: 'created',
      examined: '90000',
      kept: '87118',
      selectivity: '96.8%',
    },
    evidence: [
      { key: 'rowsRead', nodeId: 3, value: 90000, unit: 'rows', actual: true },
      { key: 'rowsKept', nodeId: 3, value: 87118, unit: 'rows', actual: true },
      { key: 'selectivity', nodeId: 3, value: 96.8, unit: 'pct', actual: true },
      { key: 'selfTime', nodeId: 3, value: 11.063, unit: 'ms', actual: true },
    ],
    actions: [
      createIndex(
        'CREATE INDEX CONCURRENTLY IF NOT EXISTS ix_orders_created ON public.orders (created);',
        'ix_orders_created',
        'orders',
      ),
    ],
    caveats: [
      { key: 'writeOverhead', verify: false },
      { key: 'selectivityHigh', verify: true },
    ],
    impact: { basis: 'time', share: 0.319, score: 0.319 },
    fromWarnings: [],
    fingerprint: 'indexforfilter|public.orders|created',
  },
  {
    id: 'pgFunctionOnColumn:77be0d11',
    rule: 'pgFunctionOnColumn',
    severity: 'warning',
    confidence: 'high',
    nodeIds: [3],
    table: PG_ORDERS,
    params: { function: 'created::date', column: 'created', examined: '90000' },
    evidence: [{ key: 'expression', nodeId: 3, text: '(created)::date', actual: false }],
    actions: [
      {
        kind: 'rewrite',
        labelKey: 'rewriteAsRange',
        sql: "created >= '<start>' AND created < '<start>'::timestamp + INTERVAL '1 day'",
        template: true,
        scope: 'query',
      },
      createIndex(
        'CREATE INDEX CONCURRENTLY IF NOT EXISTS ix_orders_created_date ON public.orders (((created)::date));',
        'ix_orders_created_date',
        'orders',
      ),
    ],
    caveats: [{ key: 'expressionMustBeImmutable', verify: true }],
    impact: { basis: 'time', share: 0.319, score: 0.223 },
    fromWarnings: [],
    statementRange: { start: 40, end: 56 },
    fingerprint: 'pgfunctiononcolumn|public.orders|created::date',
  },
];

const pgEstimated: PlanAdvice[] = [
  {
    id: 'fullScanReview:a1b2c3d4',
    rule: 'fullScanReview',
    severity: 'warning',
    confidence: 'low',
    nodeIds: [2],
    table: { database: null, schema: 'public', name: 'orders', alias: null, path: null },
    params: { table: 'public.orders', rows: '152867' },
    evidence: [{ key: 'rowsRead', nodeId: 2, value: 152867, unit: 'rows', actual: false }],
    actions: [
      {
        kind: 'check',
        labelKey: 'reviewFilter',
        template: false,
        scope: 'query',
      },
    ],
    caveats: [{ key: 'estimatedOnly', verify: true }],
    impact: { basis: 'cost', share: 0.62, score: 0.25 },
    fromWarnings: ['fullScanLargeTable'],
    fingerprint: 'fullscanreview|public.orders|',
  },
  {
    id: 'staleStatistics:4d4d4d4d',
    rule: 'staleStatistics',
    severity: 'info',
    confidence: 'medium',
    nodeIds: [4],
    table: { database: null, schema: 'public', name: 'customers', alias: 'c', path: null },
    params: {
      table: 'public.customers',
      factor: '12.5',
      estimated: '500',
      actual: '40',
      direction: 'over',
    },
    evidence: [
      { key: 'estimatedRows', nodeId: 4, value: 500, unit: 'rows', actual: false },
      { key: 'actualRows', nodeId: 4, value: 40, unit: 'rows', actual: true },
    ],
    actions: [analyzeAction('ANALYZE public.customers;', 'customers')],
    caveats: [],
    impact: { basis: 'cost', share: 0.04, score: 0.015 },
    fromWarnings: [],
    fingerprint: 'stalestatistics|public.customers|',
  },
  {
    id: 'selectStar:5e5e5e5e',
    rule: 'selectStar',
    severity: 'info',
    confidence: 'medium',
    nodeIds: [6],
    params: { sign: 'heapFetches' },
    evidence: [{ key: 'planSign', nodeId: 6, text: 'heapFetches', actual: false }],
    actions: [{ kind: 'rewrite', labelKey: 'listColumns', template: false, scope: 'query' }],
    caveats: [],
    impact: { basis: 'none', share: null, score: 0.012 },
    fromWarnings: [],
    statementRange: { start: 7, end: 8 },
    fingerprint: 'selectstar||star',
  },
];

const mysqlJson: PlanAdvice[] = [
  {
    id: 'fullScanReview:6a6a6a6a',
    rule: 'fullScanReview',
    severity: 'warning',
    confidence: 'low',
    nodeIds: [3],
    table: { database: 'dbstudio', schema: null, name: 'orders', alias: 'o', path: null },
    params: { table: 'orders', rows: '100270' },
    evidence: [{ key: 'rowsRead', nodeId: 3, value: 100270, unit: 'rows', actual: false }],
    actions: [{ kind: 'check', labelKey: 'reviewFilter', template: false, scope: 'query' }],
    caveats: [{ key: 'estimatedOnly', verify: true }],
    impact: { basis: 'cost', share: 0.45, score: 0.18 },
    fromWarnings: ['fullScanLargeTable'],
    fingerprint: 'fullscanreview|orders|',
  },
];

const mysqlTree: PlanAdvice[] = [
  {
    id: 'nestedLoopInnerScan:7b7b7b7b',
    rule: 'nestedLoopInnerScan',
    severity: 'info',
    confidence: 'low',
    nodeIds: [3],
    params: { loops: '58079' },
    evidence: [{ key: 'loops', nodeId: 6, value: 58079, unit: 'count', actual: true }],
    actions: [{ kind: 'check', labelKey: 'reviewJoinStrategy', template: false, scope: 'query' }],
    caveats: [],
    impact: { basis: 'time', share: 0.44, score: 0.1 },
    fromWarnings: ['nestedLoopManyRows'],
    fingerprint: 'nestedloopinnerscan||node:nested loop inner join',
  },
  {
    id: 'largeOffset:8c8c8c8c',
    rule: 'largeOffset',
    severity: 'warning',
    confidence: 'high',
    nodeIds: [],
    params: { offset: '50000' },
    evidence: [{ key: 'offset', value: 50000, unit: 'rows', actual: false }],
    actions: [
      {
        kind: 'rewrite',
        labelKey: 'keysetPagination',
        sql: 'WHERE (created, id) > (:last_created, :last_id)\nORDER BY created, id\nLIMIT 50',
        template: true,
        scope: 'query',
      },
    ],
    caveats: [{ key: 'keysetNeedsUniqueOrder', verify: false }],
    impact: { basis: 'none', share: null, score: 0.02 },
    fromWarnings: [],
    statementRange: { start: 120, end: 132 },
    fingerprint: 'largeoffset||offset',
  },
];

const mssqlEstimated: PlanAdvice[] = [
  {
    id: 'mssqlMissingIndex:9d9d9d9d',
    rule: 'mssqlMissingIndex',
    severity: 'warning',
    confidence: 'medium',
    nodeIds: [0],
    table: {
      database: 'AdventureWorks',
      schema: 'Sales',
      name: 'SalesOrderHeader',
      alias: 'soh',
      path: null,
    },
    params: { table: 'Sales.SalesOrderHeader', columns: 'OrderDate', impact: '82.4' },
    evidence: [{ key: 'missingIndexImpact', nodeId: 0, value: 82.4, unit: 'pct', actual: false }],
    actions: [
      createIndex(
        'CREATE NONCLUSTERED INDEX [IX_SalesOrderHeader_OrderDate] ON [Sales].[SalesOrderHeader] ([OrderDate]) INCLUDE ([TotalDue]) WITH (ONLINE = ON);',
        'IX_SalesOrderHeader_OrderDate',
        'SalesOrderHeader',
      ),
    ],
    caveats: [
      { key: 'serverSuggestion', verify: true },
      { key: 'onlineDdlMayFail', verify: false },
    ],
    impact: { basis: 'cost', share: 1, score: 0.7 },
    fromWarnings: [],
    fingerprint: 'mssqlmissingindex|sales.salesorderheader|orderdate',
  },
];

const mssqlActual: PlanAdvice[] = [
  {
    id: 'mssqlKeyLookup:aeaeaeae',
    rule: 'mssqlKeyLookup',
    severity: 'critical',
    confidence: 'high',
    nodeIds: [5, 4],
    table: { database: null, schema: 'Sales', name: 'SalesOrderDetail', alias: 'sod', path: null },
    params: { table: 'Sales.SalesOrderDetail', columns: 'OrderQty', include: 'UnitPrice' },
    evidence: [
      { key: 'lookupExecutions', nodeId: 5, value: 121600, unit: 'count', actual: true },
      { key: 'selfTime', nodeId: 5, value: 90, unit: 'ms', actual: true },
    ],
    actions: [
      createIndex(
        'CREATE NONCLUSTERED INDEX [IX_SalesOrderDetail_OrderQty] ON [Sales].[SalesOrderDetail] ([OrderQty]) INCLUDE ([UnitPrice]) WITH (DROP_EXISTING = ON, ONLINE = ON);',
        'IX_SalesOrderDetail_OrderQty',
        'SalesOrderDetail',
      ),
    ],
    caveats: [
      { key: 'extendsIndex', verify: false, params: { index: 'IX_SalesOrderDetail_OrderQty' } },
    ],
    impact: { basis: 'time', share: 0.51, score: 0.66 },
    fromWarnings: [],
    fingerprint: 'mssqlkeylookup|sales.salesorderdetail|orderqty',
  },
];

const oracleEstimated: PlanAdvice[] = [
  {
    id: 'indexForFilter:b1b1b1b1',
    rule: 'indexForFilter',
    severity: 'warning',
    confidence: 'low',
    nodeIds: [1],
    table: { database: null, schema: null, name: 'ORDERS', alias: null, path: null },
    params: { table: 'ORDERS', columns: 'STATUS' },
    evidence: [{ key: 'rowsRead', nodeId: 1, value: 1200000, unit: 'rows', actual: false }],
    actions: [
      createIndex(
        'CREATE INDEX "IX_ORDERS_STATUS" ON "ORDERS" ("STATUS") ONLINE;',
        'IX_ORDERS_STATUS',
        'ORDERS',
      ),
    ],
    caveats: [
      { key: 'estimatedOnly', verify: true },
      { key: 'metadataUnavailable', verify: true },
    ],
    impact: { basis: 'cost', share: 0.9, score: 0.3 },
    fromWarnings: ['fullScanLargeTable'],
    fingerprint: 'indexforfilter|.orders|status',
  },
];

const oracleActual: PlanAdvice[] = [
  {
    id: 'staleStatistics:c2c2c2c2',
    rule: 'staleStatistics',
    severity: 'critical',
    confidence: 'medium',
    nodeIds: [1, 0],
    table: { database: null, schema: null, name: 'ORDER_LINES', alias: null, path: null },
    params: { factor: '1080', estimated: '5', actual: '5400', direction: 'under' },
    evidence: [
      { key: 'estimatedRows', nodeId: 1, value: 5, unit: 'rows', actual: false },
      { key: 'actualRows', nodeId: 1, value: 5400, unit: 'rows', actual: true },
    ],
    actions: [
      {
        kind: 'statistics',
        labelKey: 'gatherStatistics',
        sql: "BEGIN\n  DBMS_STATS.GATHER_TABLE_STATS(ownname => USER, tabname => 'ORDER_LINES', cascade => TRUE);\nEND;\n/",
        template: false,
        scope: 'object',
        lock: 'none',
      },
    ],
    caveats: [],
    impact: { basis: 'time', share: 0.38, score: 0.5 },
    fromWarnings: ['misestimate'],
    fingerprint: 'stalestatistics|.order_lines|',
  },
  {
    id: 'hashSpill:d3d3d3d3',
    rule: 'hashSpill',
    severity: 'warning',
    confidence: 'low',
    nodeIds: [1],
    params: {},
    evidence: [{ key: 'spillDetail', nodeId: 1, text: 'tempseg 83886080 bytes', actual: true }],
    actions: [
      {
        kind: 'serverSetting',
        labelKey: 'reviewPgaTarget',
        template: false,
        scope: 'server',
      },
    ],
    caveats: [{ key: 'workMemMultiplies', verify: false }],
    impact: { basis: 'time', share: 0.38, score: 0.15 },
    fromWarnings: ['spillToDisk'],
    fingerprint: 'hashspill||node:hash group by',
  },
];

const mongoEstimated: PlanAdvice[] = [
  {
    id: 'mongoCollscan:e4e4e4e4',
    rule: 'mongoCollscan',
    severity: 'warning',
    confidence: 'low',
    nodeIds: [0],
    table: { database: 'shop', schema: null, name: 'orders', alias: null, path: null },
    params: { collection: 'shop.orders', columns: 'status' },
    evidence: [],
    actions: [
      {
        kind: 'createIndex',
        labelKey: 'createIndex',
        sql: 'db.getSiblingDB("shop").orders.createIndex({ status: 1 }, { name: "status_1" })',
        template: false,
        scope: 'object',
        lock: 'shortLock',
      },
    ],
    caveats: [{ key: 'estimatedOnly', verify: true }],
    impact: { basis: 'none', share: null, score: 0.01 },
    fromWarnings: [],
    fingerprint: 'mongocollscan|shop.orders|status',
  },
];

const mongoActual: PlanAdvice[] = [
  {
    id: 'mongoBlockingSort:f5f5f5f5',
    rule: 'mongoBlockingSort',
    severity: 'critical',
    confidence: 'high',
    nodeIds: [0, 1],
    table: { database: null, schema: null, name: 'events', alias: null, path: null },
    params: { collection: 'events', columns: 'kind, ts' },
    evidence: [
      { key: 'sortRows', nodeId: 0, value: 250000, unit: 'rows', actual: true },
      { key: 'selfTime', nodeId: 0, value: 1350, unit: 'ms', actual: true },
    ],
    actions: [
      {
        kind: 'createIndex',
        labelKey: 'createIndex',
        sql: 'db.getCollection("events").createIndex({ kind: 1, ts: -1 }, { name: "kind_1_ts_-1" })',
        template: false,
        scope: 'object',
        lock: 'shortLock',
      },
    ],
    caveats: [{ key: 'writeOverhead', verify: false }],
    impact: { basis: 'time', share: 0.54, score: 0.7 },
    fromWarnings: ['spillToDisk', 'fullScanLargeTable'],
    fingerprint: 'mongoblockingsort|events|kind,ts',
  },
];

/** Advice by plan file name (the same keys as `planTrees.json`). */
export const SAMPLE_ADVICE: Record<string, PlanAdvice[]> = {
  'pg_02_hashjoin_agg_analyze.json': pgActual,
  'pg_08_cte_subplan.json': pgEstimated,
  'mysql_json_02_join_group.json': mysqlJson,
  'mysql_tree_01_analyze_join.txt': mysqlTree,
  'mssql_01_clustered_scan_estimated.xml': mssqlEstimated,
  'mssql_03_parallel_hash_join_actual.xml': mssqlActual,
  'oracle_01_full_scan_estimated.json': oracleEstimated,
  'oracle_04_group_by_spill_actual.json': oracleActual,
  'mongo_01_collscan_planner.json': mongoEstimated,
  'mongo_03_collscan_sort_disk_stats.json': mongoActual,
};

/** Tier-2 report of the mock: the same advice, with the catalog "checked" for one fake table. */
export function sampleReport(advice: PlanAdvice[]): AdviceReport {
  const withPath = advice.map((a) =>
    a.table && a.table.name
      ? {
          ...a,
          table: {
            ...a.table,
            path: {
              segments: [
                { kind: 'schema' as const, name: a.table.schema ?? 'public' },
                { kind: 'table' as const, name: a.table.name },
              ],
            },
          },
        }
      : a,
  );
  const first = withPath.find((a) => a.table?.path);
  return {
    advice: withPath,
    metadata: {
      state: 'checked',
      checked: first?.table ? [first.table] : [],
      failed: [],
      skipped: 0,
      elapsedMs: 42,
    },
  };
}
