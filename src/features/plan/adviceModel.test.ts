import { describe, expect, it } from 'vitest';
import keys from '../../../crates/dbstudio-plan/tests/advice_fixtures/keys.json';
import { SAMPLE_ADVICE } from '@/lib/mock/planAdvice';
import enPlan from '@/locales/en/plan.json';
import viPlan from '@/locales/vi/plan.json';
import { ADVICE_RULES } from '@/lib/types';
import {
  adviceForNode,
  adviceNodeSet,
  adviceParams,
  confidenceLabel,
  formatBytes,
  formatEvidenceValue,
  rankAdvice,
  splitStatement,
  summaryParts,
} from './adviceModel';
import { makeAdvice } from './adviceTestData';
import { adviceDiff, comparePlans } from './compareModel';
import { SAMPLES, TREES } from './planTestData';

describe('rankAdvice', () => {
  it('sorts by score, then by first node, and keeps statement-only advice last on ties', () => {
    const list = [
      makeAdvice({ id: 'low', impact: { basis: 'time', share: 0.1, score: 0.1 } }),
      makeAdvice({ id: 'noNode', nodeIds: [], impact: { basis: 'none', score: 0.5 } }),
      makeAdvice({ id: 'n5', nodeIds: [5], impact: { basis: 'time', share: 0.5, score: 0.5 } }),
      makeAdvice({ id: 'n2', nodeIds: [2], impact: { basis: 'time', share: 0.5, score: 0.5 } }),
      makeAdvice({ id: 'top', impact: { basis: 'time', share: 0.9, score: 0.9 } }),
    ];
    expect(rankAdvice(list).map((a) => a.id)).toEqual(['top', 'n2', 'n5', 'noNode', 'low']);
    expect(rankAdvice(undefined)).toEqual([]);
    // The input is not reordered.
    expect(list[0]!.id).toBe('low');
  });
});

describe('confidence label', () => {
  it('maps confidence and verify caveats to the three badges', () => {
    expect(confidenceLabel(makeAdvice({ id: 'a', confidence: 'high' }))).toBe('high');
    expect(confidenceLabel(makeAdvice({ id: 'a', confidence: 'medium' }))).toBe('medium');
    expect(confidenceLabel(makeAdvice({ id: 'a', confidence: 'low' }))).toBe('verify');
    expect(
      confidenceLabel(
        makeAdvice({
          id: 'a',
          confidence: 'high',
          caveats: [{ key: 'estimatedOnly', verify: true }],
        }),
      ),
    ).toBe('verify');
    expect(
      confidenceLabel(
        makeAdvice({
          id: 'a',
          confidence: 'high',
          caveats: [{ key: 'writeOverhead', verify: false }],
        }),
      ),
    ).toBe('high');
  });
});

describe('node helpers', () => {
  const list = [makeAdvice({ id: 'a', nodeIds: [3, 5] }), makeAdvice({ id: 'b', nodeIds: [] })];
  it('knows which nodes carry advice', () => {
    expect([...adviceNodeSet(list)].sort()).toEqual([3, 5]);
    expect(adviceForNode(list, 5).map((a) => a.id)).toEqual(['a']);
    expect(adviceForNode(list, 0)).toEqual([]);
  });
});

describe('formatting', () => {
  it('formats evidence by unit and falls back to the text', () => {
    const ev = (over: object) => ({ key: 'k', actual: true, ...over });
    expect(formatEvidenceValue(ev({ value: 90000, unit: 'rows' }))).toBe('90,000');
    expect(formatEvidenceValue(ev({ value: 11.063, unit: 'ms' }))).toBe('11.1 ms');
    expect(formatEvidenceValue(ev({ value: 96.8, unit: 'pct' }))).toBe('97%');
    expect(formatEvidenceValue(ev({ value: 8864, unit: 'kb' }))).toBe('8.7 MB');
    expect(formatEvidenceValue(ev({ value: 15.2, unit: 'ratio' }))).toBe('15.2x');
    expect(formatEvidenceValue(ev({ text: 'lower(email)' }))).toBe('lower(email)');
    expect(formatBytes(512)).toBe('512 B');
  });

  it('fills missing params with an ellipsis instead of leaving placeholders', () => {
    const p = adviceParams(makeAdvice({ id: 'a', params: { columns: 'x' } }));
    expect(p.columns).toBe('x');
    expect(p.table).toBe('orders');
    expect(p.examined).toBe('…');
  });

  it('splits a statement around a character range', () => {
    expect(splitStatement('select 😀 from t', { start: 9, end: 13 })).toEqual({
      before: 'select 😀 ',
      hit: 'from',
      after: ' t',
    });
  });

  it('builds the summary of the biggest advice', () => {
    expect(summaryParts([], null)).toBeNull();
    const top = makeAdvice({ id: 'a', impact: { basis: 'cost', share: 0.78, score: 1 } });
    expect(summaryParts([top, makeAdvice({ id: 'b' })], null)).toEqual({
      count: 2,
      share: '78%',
      basis: 'cost',
    });
  });
});

describe('adviceDiff', () => {
  const a1 = makeAdvice({ id: 'x:1', fingerprint: 'idx|orders|status' });
  const a2 = makeAdvice({ id: 'x:2', fingerprint: 'stats|orders' });
  const b2 = makeAdvice({ id: 'y:2', fingerprint: 'stats|orders', severity: 'info' });
  const b3 = makeAdvice({ id: 'y:3', fingerprint: 'sort|items' });

  it('lists resolved, new and remaining advice by fingerprint', () => {
    const d = adviceDiff([a1, a2], [b2, b3]);
    expect(d.gone.map((x) => x.id)).toEqual(['x:1']);
    expect(d.added.map((x) => x.id)).toEqual(['y:3']);
    expect(d.kept.map((k) => [k.a.id, k.b.id])).toEqual([['x:2', 'y:2']]);
  });

  it('handles missing lists and repeated fingerprints', () => {
    expect(adviceDiff(undefined, undefined)).toEqual({ gone: [], added: [], kept: [] });
    const d = adviceDiff([a1, { ...a1, id: 'dup' }], []);
    expect(d.gone).toHaveLength(1);
  });

  it('adds an advice row to the totals of a comparison', () => {
    const tree = TREES[SAMPLES.pg.file]!;
    const row = comparePlans(tree, tree, { a: [a1, a2], b: [b2] }).totals.find(
      (t) => t.key === 'advice',
    );
    expect(row).toMatchObject({ a: 2, b: 1, delta: -1 });
  });
});

describe('advice translations', () => {
  const lookup = (obj: unknown, path: string) =>
    path.split('.').reduce<unknown>((o, k) => (o as Record<string, unknown> | undefined)?.[k], obj);
  const both = { en: enPlan, vi: viPlan };

  it.each(['en', 'vi'] as const)('has a title and a reason for every rule in %s', (lang) => {
    const missing = ADVICE_RULES.flatMap((r) =>
      ['title', 'why']
        .filter((f) => !lookup(both[lang], `advice.rules.${r}.${f}`))
        .map((f) => `${r}.${f}`),
    );
    expect(missing).toEqual([]);
    expect(lookup(both[lang], 'advice.rules.unknown.title')).toBeTruthy();
  });

  it.each(['en', 'vi'] as const)(
    'translates every evidence, caveat and action key of the backend and the mock in %s',
    (lang) => {
      const sample = Object.values(SAMPLE_ADVICE).flat();
      const used = {
        evidence: new Set([
          ...keys.evidence,
          ...sample.flatMap((a) => a.evidence.map((e) => e.key)),
        ]),
        caveats: new Set([...keys.caveats, ...sample.flatMap((a) => a.caveats.map((c) => c.key))]),
        actions: new Set([
          ...keys.actions,
          ...sample.flatMap((a) => a.actions.map((x) => x.labelKey)),
        ]),
      };
      const missing = Object.entries(used).flatMap(([group, set]) =>
        [...set]
          .filter((k) => !lookup(both[lang], `advice.${group}.${k}`))
          .map((k) => `${group}.${k}`),
      );
      expect(missing).toEqual([]);
    },
  );

  it('translates every rule the backend produced', () => {
    for (const r of keys.rules) expect(ADVICE_RULES).toContain(r);
  });
});
