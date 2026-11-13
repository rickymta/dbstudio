import { beforeEach, describe, expect, it } from 'vitest';
import {
  EMPTY_LAYOUT,
  isDefaultLayout,
  layoutStorageKey,
  loadLayout,
  moveColumn,
  reconcileLayout,
  saveLayout,
  setPinned,
  setWidth,
  showAll,
  toggleHidden,
  visibleColumns,
} from './columnLayout';

const names = ['id', 'a', 'b', 'c'];

describe('column layout logic', () => {
  it('starts as the natural order', () => {
    expect(visibleColumns(EMPTY_LAYOUT, names)).toEqual({ pinned: [], free: names });
    expect(isDefaultLayout(EMPTY_LAYOUT, names)).toBe(true);
  });

  it('hides, shows all and pins by name', () => {
    let l = toggleHidden(EMPTY_LAYOUT, 'a');
    l = setPinned(l, 'c', true);
    expect(visibleColumns(l, names)).toEqual({ pinned: ['c'], free: ['id', 'b'] });
    expect(visibleColumns(setPinned(l, 'c', false), names).pinned).toEqual([]);
    expect(visibleColumns(showAll(l), names).free).toEqual(['id', 'a', 'b']);
    expect(isDefaultLayout(l, names)).toBe(false);
  });

  it('moves columns before/after a target', () => {
    const l = reconcileLayout(EMPTY_LAYOUT, names);
    expect(moveColumn(l, 'c', 'id').order).toEqual(['c', 'id', 'a', 'b']);
    expect(moveColumn(l, 'id', 'b', 'after').order).toEqual(['a', 'b', 'id', 'c']);
    expect(moveColumn(l, 'a', 'a')).toBe(l);
    expect(moveColumn(l, 'a', 'zzz')).toBe(l);
  });

  it('pinned columns keep their relative display order', () => {
    let l = reconcileLayout(EMPTY_LAYOUT, names);
    l = setPinned(setPinned(l, 'b', true), 'id', true);
    expect(visibleColumns(l, names).pinned).toEqual(['id', 'b']);
    l = moveColumn(l, 'b', 'id');
    expect(visibleColumns(l, names).pinned).toEqual(['b', 'id']);
  });

  it('appends new columns at the end and drops columns that disappeared', () => {
    let l = moveColumn(reconcileLayout(EMPTY_LAYOUT, names), 'c', 'id');
    l = setWidth(toggleHidden(setPinned(l, 'b', true), 'a'), 'a', 200);
    l = { ...l, widths: { ...l.widths, gone: 100 }, hidden: [...l.hidden, 'gone'] };
    const next = reconcileLayout(l, ['id', 'b', 'c', 'new']);
    expect(next.order).toEqual(['c', 'id', 'b', 'new']);
    expect(next.hidden).toEqual([]);
    expect(next.pinned).toEqual(['b']);
    expect(next.widths).toEqual({});
    expect(visibleColumns(l, ['id', 'b', 'c', 'new']).free).toEqual(['c', 'id', 'new']);
  });

  it('clamps widths', () => {
    expect(setWidth(EMPTY_LAYOUT, 'a', 1).widths.a).toBe(56);
  });
});

describe('layout persistence', () => {
  beforeEach(() => localStorage.clear());

  it('round-trips per table key', () => {
    const k1 = layoutStorageKey('conn', '["a"]');
    const k2 = layoutStorageKey('conn', '["b"]');
    const l = setWidth(setPinned(toggleHidden(EMPTY_LAYOUT, 'a'), 'b', true), 'c', 150);
    saveLayout(k1, l);
    expect(loadLayout(k1)).toEqual(l);
    expect(loadLayout(k2)).toEqual(EMPTY_LAYOUT);
  });

  it('ignores corrupt or malformed stored data', () => {
    const k = layoutStorageKey('c', 'p');
    localStorage.setItem(k, '{not json');
    expect(loadLayout(k)).toEqual(EMPTY_LAYOUT);
    localStorage.setItem(
      k,
      JSON.stringify({ order: [1], hidden: 'x', pinned: ['a'], widths: { a: 'w', b: 90 } }),
    );
    expect(loadLayout(k)).toEqual({ order: [], hidden: [], pinned: ['a'], widths: { b: 90 } });
  });
});
