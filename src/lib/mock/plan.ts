// Mock query plans: answers with captured plans of the Rust fixtures
// (crates/dbstudio-plan/tests/fixtures). The matching trees were produced by `dbstudio-plan`
// and live in planTrees.json, so the browser mock needs no parser of its own.

import type {
  AdviceReport,
  DbKind,
  ExplainRequest,
  PlanFormat,
  PlanHistoryEntry,
  PlanHistoryRecord,
  PlanResult,
  PlanRules,
  PlanTree,
  RawPlan,
  Uuid,
} from '../types';
import mysqlJson from '../../../crates/dbstudio-plan/tests/fixtures/mysql_json_02_join_group.json?raw';
import mongoEstimated from '../../../crates/dbstudio-plan/tests/fixtures/mongo_01_collscan_planner.json?raw';
import mongoActual from '../../../crates/dbstudio-plan/tests/fixtures/mongo_03_collscan_sort_disk_stats.json?raw';
import mssqlEstimated from '../../../crates/dbstudio-plan/tests/fixtures/mssql_01_clustered_scan_estimated.xml?raw';
import mssqlActual from '../../../crates/dbstudio-plan/tests/fixtures/mssql_03_parallel_hash_join_actual.xml?raw';
import mysqlTree from '../../../crates/dbstudio-plan/tests/fixtures/mysql_tree_01_analyze_join.txt?raw';
import oracleEstimated from '../../../crates/dbstudio-plan/tests/fixtures/oracle_01_full_scan_estimated.json?raw';
import oracleActual from '../../../crates/dbstudio-plan/tests/fixtures/oracle_04_group_by_spill_actual.json?raw';
import pgActual from '../../../crates/dbstudio-plan/tests/fixtures/pg_02_hashjoin_agg_analyze.json?raw';
import pgEstimated from '../../../crates/dbstudio-plan/tests/fixtures/pg_08_cte_subplan.json?raw';
import { SAMPLE_ADVICE, sampleReport } from './planAdvice';
import trees from './planTrees.json';
import { mockSessionConnectionId, mockSessionKind } from './sessions';
import { ipcError, newId, sleep, unsupported } from './util';

interface Sample {
  file: string;
  format: PlanFormat;
  text: string;
}

const SAMPLES: Partial<Record<DbKind, { estimated: Sample; actual: Sample } | null>> = {
  postgres: {
    estimated: { file: 'pg_08_cte_subplan.json', format: 'postgresJson', text: pgEstimated },
    actual: { file: 'pg_02_hashjoin_agg_analyze.json', format: 'postgresJson', text: pgActual },
  },
  mysql: {
    estimated: { file: 'mysql_json_02_join_group.json', format: 'mySqlJson', text: mysqlJson },
    actual: { file: 'mysql_tree_01_analyze_join.txt', format: 'mySqlTree', text: mysqlTree },
  },
  sqlserver: {
    estimated: {
      file: 'mssql_01_clustered_scan_estimated.xml',
      format: 'msSqlShowplanXml',
      text: mssqlEstimated,
    },
    actual: {
      file: 'mssql_03_parallel_hash_join_actual.xml',
      format: 'msSqlShowplanXml',
      text: mssqlActual,
    },
  },
  oracle: {
    estimated: {
      file: 'oracle_01_full_scan_estimated.json',
      format: 'oraclePlanRows',
      text: oracleEstimated,
    },
    actual: {
      file: 'oracle_04_group_by_spill_actual.json',
      format: 'oraclePlanRows',
      text: oracleActual,
    },
  },
  mongodb: {
    estimated: {
      file: 'mongo_01_collscan_planner.json',
      format: 'mongoExplain',
      text: mongoEstimated,
    },
    actual: {
      file: 'mongo_03_collscan_sort_disk_stats.json',
      format: 'mongoExplain',
      text: mongoActual,
    },
  },
  redis: null,
};

const TREES = trees as unknown as Record<string, PlanTree>;

function treeOf(file: string): PlanTree {
  return { ...structuredClone(TREES[file]!), advice: structuredClone(SAMPLE_ADVICE[file] ?? []) };
}

function allSamples(): Sample[] {
  return Object.values(SAMPLES).flatMap((s) => (s ? [s.estimated, s.actual] : []));
}

/** Last plan saved through the mock "Save" dialog, returned by the mock "Open" dialog. */
let savedPlan: RawPlan | null = null;

/** In-memory plan history (newest last); every mock `planExplain` records one entry. */
let planHistory: PlanHistoryRecord[] = [];

export function resetMockPlanHistory(): void {
  planHistory = [];
  savedPlan = null;
}

function entryOf(record: PlanHistoryRecord): PlanHistoryEntry {
  const { rawPlan, ...entry } = record;
  void rawPlan;
  return entry;
}

export const planMock = {
  async planExplain(
    sessionId: Uuid,
    leaseId: Uuid | null,
    request: ExplainRequest,
    rules: PlanRules | null,
  ): Promise<PlanResult> {
    const kind = mockSessionKind(sessionId);
    void [leaseId, rules];
    const samples = SAMPLES[kind];
    if (!samples) throw unsupported('explain');
    await sleep(120, 320);
    if (request.text.trim() === '') {
      throw ipcError('query', 'nothing to explain');
    }
    const sample = request.analyze ? samples.actual : samples.estimated;
    const raw: RawPlan = {
      format: sample.format,
      text: sample.text,
      statement: request.text,
      analyzed: request.analyze ?? false,
      rolledBack: request.analyze ?? false,
      serverVersion: 'mock',
      capturedAt: new Date().toISOString(),
    };
    planHistory.push({
      id: newId(),
      connectionId: mockSessionConnectionId(sessionId) ?? null,
      createdAt: raw.capturedAt,
      database: request.database ?? null,
      statement: request.text,
      analyze: request.analyze ?? false,
      format: raw.format,
      totalCost: null,
      totalMs: null,
      warnings: 0,
      rawPlan: structuredClone(raw),
    });
    return { raw, tree: treeOf(sample.file), parseError: null };
  },
  async planParse(raw: RawPlan, rules: PlanRules | null): Promise<PlanTree> {
    void rules;
    await sleep(20, 60);
    const sample = allSamples().find((s) => s.text === raw.text && s.format === raw.format);
    if (!sample) throw ipcError('other', 'invalid plan: unknown to the browser mock');
    return treeOf(sample.file);
  },
  /** Tier 2 of the Query Advisor: the sample advice with one fake table "checked". */
  async planAdvise(
    sessionId: Uuid,
    leaseId: Uuid | null,
    raw: RawPlan,
    database: string | null,
  ): Promise<AdviceReport> {
    void [leaseId, database];
    mockSessionKind(sessionId);
    await sleep(40, 120);
    const sample = allSamples().find((s) => s.text === raw.text && s.format === raw.format);
    if (!sample) throw ipcError('other', 'invalid plan: unknown to the browser mock');
    return sampleReport(structuredClone(SAMPLE_ADVICE[sample.file] ?? []));
  },
  async planFileSave(raw: RawPlan): Promise<string | null> {
    savedPlan = structuredClone(raw);
    return 'plan.dbplan.json';
  },
  async planFileOpen(): Promise<RawPlan | null> {
    return savedPlan ? structuredClone(savedPlan) : null;
  },
  /** Newest first; `before` is the `createdAt` of the last entry of the previous page. */
  async planHistoryList(
    connectionId: Uuid | null,
    limit: number,
    before: string | null,
  ): Promise<PlanHistoryEntry[]> {
    await sleep(5, 20);
    return planHistory
      .filter((r) => connectionId === null || r.connectionId === connectionId)
      .filter((r) => before === null || r.createdAt < before)
      .slice()
      .reverse()
      .slice(0, limit)
      .map(entryOf);
  },
  async planHistoryGet(id: Uuid): Promise<PlanHistoryRecord> {
    await sleep(5, 20);
    const record = planHistory.find((r) => r.id === id);
    if (!record) throw ipcError('notFound', `plan history ${id}`);
    return structuredClone(record);
  },
  async planHistoryDelete(id: Uuid): Promise<void> {
    await sleep(5, 20);
    planHistory = planHistory.filter((r) => r.id !== id);
  },
  /** Resolves to the number of deleted entries. */
  async planHistoryClear(connectionId: Uuid | null): Promise<number> {
    await sleep(5, 20);
    const before = planHistory.length;
    planHistory = planHistory.filter(
      (r) => connectionId !== null && r.connectionId !== connectionId,
    );
    return before - planHistory.length;
  },
};
