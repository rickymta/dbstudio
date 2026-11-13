import { beforeEach, describe, expect, it } from 'vitest';
import type { ColumnMeta, Value } from '@/lib/types';
import { NULL_EDIT } from './cellEdit';
import {
  buildChanges,
  changeTargets,
  countChanges,
  extractKey,
  rowKeyString,
  usePendingChanges,
} from './pendingChanges';
import { duplicateCells } from './rowOps';
import '../store'; // registers the close guard
import { getTabGuard } from '@/stores/tabGuard';

const TAB = 't1';
const k = (n: number): Value[] => [{ t: 'int', v: n }];
const text = (v: string) => ({ kind: 'text' as const, v });
const st = () => usePendingChanges.getState();
const pending = () => st().byTab[TAB]!;

beforeEach(() => usePendingChanges.setState({ byTab: {}, nextInsertId: 1 }));

describe('pending changes store', () => {
  it('builds deletes, then updates, then inserts', () => {
    st().addInsert(TAB, { a: text('new') });
    st().setUpdateCell(TAB, k(2), 'a', text('x'));
    st().toggleDelete(TAB, k(1));
    st().setUpdateCell(TAB, k(2), 'b', NULL_EDIT);
    const changes = buildChanges(pending());
    expect(changes.map((c) => c.op)).toEqual(['delete', 'update', 'insert']);
    expect(changes[1]).toEqual({
      op: 'update',
      key: k(2),
      set: [
        { column: 'a', value: text('x') },
        { column: 'b', value: NULL_EDIT },
      ],
    });
    expect(changeTargets(pending()).map((t) => t.kind)).toEqual(['delete', 'update', 'insert']);
    expect(countChanges(pending())).toBe(3);
  });

  it('reverting the last cell removes the update; an empty tab is dropped', () => {
    st().setUpdateCell(TAB, k(1), 'a', text('x'));
    expect(countChanges(pending())).toBe(1);
    st().setUpdateCell(TAB, k(1), 'a', null);
    expect(st().byTab[TAB]).toBeUndefined();
  });

  it('a deleted row drops its updates from the change list, and undoing delete brings them back', () => {
    st().setUpdateCell(TAB, k(1), 'a', text('x'));
    st().toggleDelete(TAB, k(1));
    expect(buildChanges(pending()).map((c) => c.op)).toEqual(['delete']);
    expect(countChanges(pending())).toBe(1);
    st().toggleDelete(TAB, k(1));
    expect(buildChanges(pending()).map((c) => c.op)).toEqual(['update']);
  });

  it('keeps NULL and the empty text as different values', () => {
    st().setUpdateCell(TAB, k(1), 'a', text(''));
    st().setUpdateCell(TAB, k(1), 'b', NULL_EDIT);
    const [c] = buildChanges(pending());
    expect(c).toMatchObject({
      set: [
        { column: 'a', value: { kind: 'text', v: '' } },
        { column: 'b', value: { kind: 'value', v: { t: 'null' } } },
      ],
    });
  });

  it('inserts get stable ids and can be edited or removed', () => {
    const a = st().addInsert(TAB, {});
    const b = st().addInsert(TAB, { x: text('1') });
    st().setInsertCell(TAB, a, 'x', text('2'));
    st().removeInsert(TAB, b);
    expect(pending().inserts).toEqual([{ id: a, cells: { x: text('2') } }]);
    st().discard(TAB);
    expect(st().byTab[TAB]).toBeUndefined();
  });
});

describe('keys and row operations', () => {
  const cols: ColumnMeta[] = [
    { name: 'id', typeName: 'int', valueKind: 'int' },
    { name: 'a', typeName: 'text', valueKind: 'text' },
    { name: 'g', typeName: 'int', valueKind: 'int' },
  ];
  const row: Value[] = [{ t: 'int', v: 7 }, { t: 'null' }, { t: 'int', v: 2 }];

  it('extracts the key by column name', () => {
    expect(extractKey(row, cols, ['id'])).toEqual(k(7));
    expect(extractKey(row, cols, ['nope'])).toBeNull();
    expect(extractKey(row, cols, [])).toBeNull();
    expect(rowKeyString(k(7))).toBe('[{"t":"int","v":7}]');
  });

  it('a duplicate keeps array values typed', () => {
    const arr: Value = { t: 'array', v: [{ t: 'int', v: 1 }] };
    expect(
      duplicateCells([arr], [{ name: 'tags', typeName: 'int[]', valueKind: 'array' }], [], []),
    ).toEqual({
      tags: { kind: 'value', v: arr },
    });
  });

  it('a duplicate leaves out generated columns, defaults the key and keeps NULL', () => {
    expect(duplicateCells(row, cols, ['id'], ['g'])).toEqual({
      id: { kind: 'default' },
      a: { kind: 'value', v: { t: 'null' } },
    });
  });
});

describe('close guard', () => {
  it('exists for any tab with pending changes, mounted or not, and goes away with them', () => {
    expect(getTabGuard(TAB)).toBeUndefined();
    st().toggleDelete(TAB, k(1));
    expect(getTabGuard(TAB)).toBeDefined();
    st().toggleDelete(TAB, k(1));
    expect(getTabGuard(TAB)).toBeUndefined();
    st().addInsert(TAB, {});
    expect(getTabGuard(TAB)).toBeDefined();
    st().discard(TAB);
    expect(getTabGuard(TAB)).toBeUndefined();
  });
});
