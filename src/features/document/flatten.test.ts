import { describe, expect, it } from 'vitest';
import type { Value } from '@/lib/types';
import {
  ColumnTracker,
  docId,
  fieldOf,
  docFields,
  fromDotPath,
  inferColumns,
  summarizeCell,
  summaryText,
  toDotPath,
  valueAtPath,
} from './flatten';

const doc = (...pairs: [string, Value][]): Value => ({ t: 'document', v: pairs });
const int = (n: number): Value => ({ t: 'int', v: n });
const text = (s: string): Value => ({ t: 'text', v: s });
const oid = (s: string): Value => ({ t: 'ext', v: { type: 'objectId', text: s } });

describe('inferColumns', () => {
  it('unions keys of non-uniform documents in order of first appearance', () => {
    const docs = [
      doc(['_id', oid('a')], ['name', text('x')]),
      doc(['_id', oid('b')], ['age', int(3)], ['name', text('y')]),
      doc(['_id', oid('c')], ['late', int(1)]),
    ];
    const { columns, truncated } = inferColumns(docs);
    expect(columns.map((c) => c.name)).toEqual(['_id', 'name', 'age', 'late']);
    expect(truncated).toBe(false);
  });

  it('puts _id first even when it shows up late', () => {
    const { columns } = inferColumns([doc(['a', int(1)]), doc(['b', int(1)], ['_id', int(1)])]);
    expect(columns.map((c) => c.name)).toEqual(['_id', 'a', 'b']);
  });

  it('caps the number of columns and flags it', () => {
    const wide = doc(...Array.from({ length: 12 }, (_, i): [string, Value] => [`k${i}`, int(i)]));
    const { columns, truncated } = inferColumns([wide], 5);
    expect(columns).toHaveLength(5);
    expect(truncated).toBe(true);
  });

  it('marks keys that cannot be a dotted path as not sortable', () => {
    const { columns } = inferColumns([
      doc(['a.b', int(1)]),
      doc(['$x', int(1)]),
      doc(['ok', int(1)]),
    ]);
    expect(columns.map((c) => c.sortable)).toEqual([false, false, true]);
  });

  it('ignores rows that are not documents', () => {
    expect(inferColumns([{ t: 'null' }, doc(['a', int(1)])]).columns).toHaveLength(1);
  });
});

describe('ColumnTracker', () => {
  it('scans appended rows only and resets on reload', () => {
    const t = new ColumnTracker();
    const rows: Value[][] = [[doc(['a', int(1)])]];
    expect(t.update(rows)).toBe(true);
    expect(t.update(rows)).toBe(false);
    rows.push([doc(['a', int(2)], ['b', int(2)])]);
    expect(t.update(rows)).toBe(true);
    expect(t.columns.map((c) => c.name)).toEqual(['a', 'b']);
    expect(t.update([[doc(['z', int(1)])]])).toBe(true);
    expect(t.columns.map((c) => c.name)).toEqual(['z']);
  });
});

describe('cells', () => {
  const d = doc(
    ['a', { t: 'null' }],
    ['n', doc(['x', int(1)], ['y', int(2)])],
    ['l', { t: 'array', v: [int(1)] }],
  );
  const f = docFields(d)!;

  it('tells a missing key from null', () => {
    expect(summarizeCell(fieldOf(f, 'nope'))).toEqual({ kind: 'missing' });
    expect(summarizeCell(fieldOf(f, 'a'))).toEqual({ kind: 'null' });
  });

  it('summarizes nested values', () => {
    expect(summaryText(summarizeCell(fieldOf(f, 'n')))).toBe('{2 fields}');
    expect(summaryText(summarizeCell(fieldOf(f, 'l')))).toBe('[1 item]');
    expect(summaryText(summarizeCell(int(1)))).toBeNull();
  });

  it('reads _id and dotted paths', () => {
    expect(docId(doc(['_id', int(7)]))).toEqual(int(7));
    expect(docId(doc(['a', int(7)]))).toBeUndefined();
    expect(valueAtPath(d, ['n', 'y'])).toEqual(int(2));
    expect(valueAtPath(d, ['l', '0'])).toEqual(int(1));
    expect(valueAtPath(d, ['n', 'zz'])).toBeUndefined();
    expect(toDotPath(['address', 'city'])).toBe('address.city');
    expect(fromDotPath('address.city')).toEqual(['address', 'city']);
  });
});
