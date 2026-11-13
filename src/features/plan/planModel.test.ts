import { describe, expect, it } from 'vitest';
import type { PlanNode, RawPlan } from '@/lib/types';
import {
  explainErrorMessage,
  flatten,
  formatCost,
  formatCount,
  formatDuration,
  isWriteStatement,
  locateRaw,
  maxSeverity,
  metricOf,
  nodeById,
  nodeShare,
  parentMap,
  pathTo,
  warningKey,
  warningsByNode,
} from './planModel';
import { rawOf, SAMPLES, TREES } from './planTestData';
import i18n from '@/lib/i18n';

const pg = TREES[SAMPLES.pg.file]!;

describe('tree helpers', () => {
  it('flattens in pre-order with ids equal to positions', () => {
    const nodes = flatten(pg.root);
    expect(nodes.map((n) => n.id)).toEqual(nodes.map((_, i) => i));
    expect(nodes).toHaveLength(pg.nodeCount);
    expect(nodeById(pg, 3)?.op).toBe('Seq Scan');
  });

  it('knows parents and paths', () => {
    expect(parentMap(pg.root).get(0)).toBeNull();
    expect(parentMap(pg.root).get(3)).toBe(2);
    expect(pathTo(pg.root, 3)).toEqual([0, 1, 2, 3]);
  });

  it('groups warnings per node and ranks severities', () => {
    const tree = TREES[SAMPLES.oracle.file]!;
    const byNode = warningsByNode(tree);
    expect(byNode.get(1)?.length).toBeGreaterThanOrEqual(2);
    expect(maxSeverity(byNode.get(1))).toBe('critical');
    expect(maxSeverity(undefined)).toBeNull();
    expect(maxSeverity([{ nodeId: 0, rule: 'spillToDisk', severity: 'info', params: {} }])).toBe(
      'info',
    );
  });

  it('measures by time when present, else by cost', () => {
    expect(metricOf(pg)).toBe('time');
    expect(metricOf(TREES['pg_08_cte_subplan.json']!)).toBe('cost');
    const share = nodeShare(pg, nodeById(pg, 3)!)!;
    expect(share).toBeGreaterThan(0);
    expect(share).toBeLessThanOrEqual(1);
  });
});

describe('formatting', () => {
  it('formats counts, costs and durations', () => {
    expect(formatCount(null)).toBe('-');
    expect(formatCount(1234)).toBe('1,234');
    expect(formatCount(12_500_000)).toBe('12.5M');
    expect(formatCost(2923.84)).toBe('2,924');
    expect(formatCost(31)).toBe('31');
    expect(formatDuration(0.0004)).toBe('0 ms');
    expect(formatDuration(0.25)).toBe('0.250 ms');
    expect(formatDuration(34.696)).toBe('34.7 ms');
    expect(formatDuration(2480)).toBe('2.48 s');
    expect(formatDuration(125_000)).toBe('2 min 5 s');
  });
});

describe('isWriteStatement', () => {
  it.each([
    ['update t set a = 1', true],
    ['  /* x */ DELETE FROM t', true],
    ['insert into t select 1', true],
    ['with x as (select 1) update t set a = 1', true],
    ['select * into new_t from t', true],
    ['create table t2 as select 1', true],
    ['select * from t where a = (select max(b) from u)', false],
    ["select 'delete me' as x, `update` from t", false],
    ['with x as (select 1) select * from x', false],
    ['db.orders.find({})', false],
    ['', false],
  ])('%s -> %s', (sql, expected) => {
    expect(isWriteStatement(sql)).toBe(expected);
  });
});

describe('locateRaw', () => {
  function covered(raw: RawPlan, node: PlanNode): string {
    const range = locateRaw(raw, node);
    expect(range, `${node.op} (${node.id})`).not.toBeNull();
    return raw.text
      .split('\n')
      .slice(range![0], range![1] + 1)
      .join('\n');
  }

  it('finds every PostgreSQL node by its line range', () => {
    const raw = rawOf(SAMPLES.pg.format, SAMPLES.pg.text);
    for (const n of flatten(pg.root)) {
      const text = covered(raw, n);
      expect(text).toContain(`"Node Type": "${n.op.replace('Parallel ', '')}"`);
    }
  });

  it('finds every SQL Server node by NodeId, spanning its RelOp', () => {
    const tree = TREES[SAMPLES.mssql.file]!;
    const raw = rawOf(SAMPLES.mssql.format, SAMPLES.mssql.text);
    for (const n of flatten(tree.root)) {
      const text = covered(raw, n);
      expect(text).toContain(`PhysicalOp="${n.op}"`);
      expect(text.trimEnd().endsWith('</RelOp>')).toBe(true);
    }
    // A parent covers its children.
    expect(covered(raw, tree.root)).toContain('PhysicalOp="Clustered Index Scan"');
  });

  it('finds Oracle rows by ID', () => {
    const tree = TREES[SAMPLES.oracle.file]!;
    const raw = rawOf(SAMPLES.oracle.format, SAMPLES.oracle.text);
    for (const n of flatten(tree.root)) {
      expect(covered(raw, n)).toContain(`"OPERATION": "${n.op.split(' ')[0]}`);
    }
  });

  it('finds MySQL tree lines and MongoDB stages', () => {
    const tree = TREES[SAMPLES.mysqlTree.file]!;
    const raw = rawOf(SAMPLES.mysqlTree.format, SAMPLES.mysqlTree.text);
    for (const n of flatten(tree.root))
      expect(covered(raw, n).toLowerCase()).toContain(n.op.toLowerCase().split(' ')[0]);
    const mongo = TREES[SAMPLES.mongo.file]!;
    const mraw = rawOf(SAMPLES.mongo.format, SAMPLES.mongo.text);
    for (const n of flatten(mongo.root)) expect(covered(mraw, n)).toContain(`"stage": "${n.op}"`);
  });

  it('returns null for unknown references', () => {
    const raw = rawOf('postgresJson', '{}');
    const node = { ...pg.root, raw: { kind: 'xmlNode', nodeId: '99' } } as PlanNode;
    expect(locateRaw(raw, node)).toBeNull();
    expect(locateRaw(raw, { ...pg.root, raw: { kind: 'none' } })).toBeNull();
    expect(locateRaw(raw, { ...pg.root, raw: { kind: 'lines', start: 50, end: 60 } })).toBeNull();
  });
});

describe('messages', () => {
  const t = i18n.getFixedT('en', 'plan');

  it('has a message for every warning rule and direction', () => {
    const rules = [
      'fullScanLargeTable',
      'misestimate',
      'spillToDisk',
      'nestedLoopManyRows',
    ] as const;
    for (const rule of rules) {
      for (const direction of ['under', 'over']) {
        const key = warningKey({ nodeId: 0, rule, severity: 'info', params: { direction } });
        expect(i18n.exists(key, { ns: 'plan' }), key).toBe(true);
      }
    }
    for (const lng of ['vi', 'en']) {
      expect(
        i18n.getFixedT(lng, 'plan')('warning.misestimateUnder', {
          estimated: 1,
          actual: 2,
          factor: 2,
        }),
      ).toContain('2');
    }
  });

  it('explains unsupported capabilities', () => {
    const e = (capability: string) =>
      explainErrorMessage(t, { kind: 'unsupported', message: capability, capability });
    expect(e('explain_analyze_writes')).toMatch(/not supported/);
    expect(e('explain_analyze_ddl')).toMatch(/Schema-changing/);
    expect(e('explain_analyze')).toMatch(/no EXPLAIN ANALYZE/);
    expect(e('whatever')).toContain('whatever');
    expect(explainErrorMessage(t, { kind: 'query', message: 'boom' })).toBe('boom');
  });
});
