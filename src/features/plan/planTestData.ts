// Shared fixtures of the plan tests (not imported by app code): captured plans of the Rust
// fixtures with the trees `dbstudio-plan` produced for them.
import mongoActual from '../../../crates/dbstudio-plan/tests/fixtures/mongo_03_collscan_sort_disk_stats.json?raw';
import mssqlActual from '../../../crates/dbstudio-plan/tests/fixtures/mssql_03_parallel_hash_join_actual.xml?raw';
import mysqlTree from '../../../crates/dbstudio-plan/tests/fixtures/mysql_tree_01_analyze_join.txt?raw';
import oracleActual from '../../../crates/dbstudio-plan/tests/fixtures/oracle_04_group_by_spill_actual.json?raw';
import pgActual from '../../../crates/dbstudio-plan/tests/fixtures/pg_02_hashjoin_agg_analyze.json?raw';
import pgSpill from '../../../crates/dbstudio-plan/tests/fixtures/pg_04_sort_spill_analyze.json?raw';
import trees from '@/lib/mock/planTrees.json';
import type { PlanFormat, PlanTree, RawPlan } from '@/lib/types';
import { usePlanStore, type PlanEntry } from './planStore';

export const TREES = trees as unknown as Record<string, PlanTree>;

export function rawOf(format: PlanFormat, text: string, statement = 'select 1'): RawPlan {
  return {
    format,
    text,
    statement,
    analyzed: true,
    rolledBack: true,
    serverVersion: '16.2',
    capturedAt: '2025-01-01T00:00:00Z',
  };
}

export const SAMPLES = {
  pg: { file: 'pg_02_hashjoin_agg_analyze.json', format: 'postgresJson', text: pgActual },
  mssql: {
    file: 'mssql_03_parallel_hash_join_actual.xml',
    format: 'msSqlShowplanXml',
    text: mssqlActual,
  },
  oracle: {
    file: 'oracle_04_group_by_spill_actual.json',
    format: 'oraclePlanRows',
    text: oracleActual,
  },
  mysqlTree: { file: 'mysql_tree_01_analyze_join.txt', format: 'mySqlTree', text: mysqlTree },
  mongo: {
    file: 'mongo_03_collscan_sort_disk_stats.json',
    format: 'mongoExplain',
    text: mongoActual,
  },
} as const satisfies Record<string, { file: string; format: PlanFormat; text: string }>;

export const PG_SPILL_TEXT = pgSpill;

/** Put a ready plan into the store and return its id. */
export function seedPlan(
  patch: Partial<PlanEntry> = {},
  sample: { file: string; format: PlanFormat; text: string } = SAMPLES.pg,
): string {
  const planId = patch.planId ?? crypto.randomUUID();
  usePlanStore.getState().add({
    planId,
    sessionId: 's1',
    connectionId: 'c1',
    leaseId: null,
    request: {
      queryId: 'q1',
      text: 'select * from orders',
      database: null,
      analyze: true,
      options: {},
      timeoutMs: null,
    },
    statement: 'select * from orders',
    raw: rawOf(sample.format, sample.text, 'select * from orders'),
    tree: structuredClone(TREES[sample.file]!),
    parseError: null,
    status: 'ready',
    error: null,
    selectedId: null,
    runId: 1,
    ...patch,
  });
  return planId;
}
