import { beforeEach, describe, expect, it } from 'vitest';
import {
  EMPTY_LAYOUT,
  getWidth,
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

  it('hides, shows all and pins by name, from an empty (never saved) layout', () => {
    let l = toggleHidden(EMPTY_LAYOUT, 'a', names);
    l = setPinned(l, 'c', true, names);
    expect(visibleColumns(l, names)).toEqual({ pinned: ['c'], free: ['id', 'b'] });
    expect(visibleColumns(setPinned(l, 'c', false, names), names).pinned).toEqual([]);
    expect(visibleColumns(showAll(l, names), names).free).toEqual(['id', 'a', 'b']);
    expect(isDefaultLayout(l, names)).toBe(false);
  });

  it('drag-reorders from an empty layout (the first drag on a table never saved)', () => {
    const l = moveColumn(EMPTY_LAYOUT, 'c', 'id', names);
    expect(l.order).toEqual(['c', 'id', 'a', 'b']);
    expect(visibleColumns(l, names).free).toEqual(['c', 'id', 'a', 'b']);
    expect(moveColumn(EMPTY_LAYOUT, 'id', 'b', names, 'after').order).toEqual([
      'a',
      'b',
      'id',
      'c',
    ]);
  });

  it('moves relative to a column that is new since the layout was saved', () => {
    const saved = { ...EMPTY_LAYOUT, order: ['id', 'a', 'b', 'c'] };
    const grown = [...names, 'fresh'];
    const l = moveColumn(saved, 'id', 'fresh', grown);
    expect(l.order).toEqual(['a', 'b', 'c', 'id', 'fresh']);
    expect(moveColumn(saved, 'fresh', 'id', grown).order).toEqual(['fresh', 'id', 'a', 'b', 'c']);
  });

  it('ignores moves involving unknown columns and same-column moves', () => {
    expect(moveColumn(EMPTY_LAYOUT, 'a', 'a', names).order).toEqual(names);
    expect(moveColumn(EMPTY_LAYOUT, 'a', 'zzz', names).order).toEqual(names);
    expect(moveColumn(EMPTY_LAYOUT, 'zzz', 'a', names).order).toEqual(names);
    expect(setPinned(EMPTY_LAYOUT, 'zzz', true, names).pinned).toEqual([]);
  });

  it('pinned columns keep their relative display order', () => {
    let l = setPinned(setPinned(EMPTY_LAYOUT, 'b', true, names), 'id', true, names);
    expect(visibleColumns(l, names).pinned).toEqual(['id', 'b']);
    l = moveColumn(l, 'b', 'id', names);
    expect(visibleColumns(l, names).pinned).toEqual(['b', 'id']);
  });

  it('appends new columns at the end and drops columns that disappeared', () => {
    let l = moveColumn(EMPTY_LAYOUT, 'c', 'id', names);
    l = setWidth(toggleHidden(setPinned(l, 'b', true, names), 'a', names), 'a', 200, names);
    l = { ...l, widths: { ...l.widths, gone: 100 }, hidden: [...l.hidden, 'gone'] };
    const next = reconcileLayout(l, ['id', 'b', 'c', 'new']);
    expect(next.order).toEqual(['c', 'id', 'b', 'new']);
    expect(next.hidden).toEqual([]);
    expect(next.pinned).toEqual(['b']);
    expect(next.widths).toEqual({});
    expect(visibleColumns(l, ['id', 'b', 'c', 'new']).free).toEqual(['c', 'id', 'new']);
  });

  it('clamps widths', () => {
    expect(getWidth(setWidth(EMPTY_LAYOUT, 'a', 1, names), 'a')).toBe(56);
    expect(getWidth(setWidth(EMPTY_LAYOUT, 'a', 1e9, names), 'a')).toBe(600);
  });
});

describe('prototype-looking column names', () => {
  const odd = ['constructor', 'toString', 'valueOf', '__proto__', 'hasOwnProperty', 'id'];

  it('never reads a width from the prototype chain', () => {
    for (const n of odd) expect(getWidth(EMPTY_LAYOUT, n)).toBeUndefined();
  });

  it('stores and reads a width for every such name, including __proto__', () => {
    let l = EMPTY_LAYOUT;
    odd.forEach((n, i) => {
      l = setWidth(l, n, 100 + i * 10, odd);
    });
    odd.forEach((n, i) => expect(getWidth(l, n)).toBe(100 + i * 10));
    expect(Object.getPrototypeOf(l.widths)).toBe(Object.prototype); // not polluted
    expect(({} as Record<string, unknown>).polluted).toBeUndefined();
  });

  it('survives reconcile, visibility and a storage round trip', () => {
    localStorage.clear();
    let l = setWidth(EMPTY_LAYOUT, '__proto__', 222, odd);
    l = toggleHidden(setPinned(l, 'constructor', true, odd), 'toString', odd);
    const key = layoutStorageKey('c', 'p');
    saveLayout(key, l);
    const back = loadLayout(key);
    expect(getWidth(back, '__proto__')).toBe(222);
    expect(visibleColumns(back, odd)).toEqual({
      pinned: ['constructor'],
      free: ['valueOf', '__proto__', 'hasOwnProperty', 'id'],
    });
  });
});

describe('layout persistence', () => {
  beforeEach(() => localStorage.clear());

  it('round-trips per table key', () => {
    const k1 = layoutStorageKey('conn', '["a"]');
    const k2 = layoutStorageKey('conn', '["b"]');
    const l = setWidth(
      setPinned(toggleHidden(EMPTY_LAYOUT, 'a', names), 'b', true, names),
      'c',
      150,
      names,
    );
    saveLayout(k1, l);
    expect(loadLayout(k1)).toEqual(l);
    expect(loadLayout(k2)).toEqual(EMPTY_LAYOUT);
  });

  it('ignores corrupt data', () => {
    const k = layoutStorageKey('c', 'p');
    for (const raw of ['{not json', 'null', '42', '"s"', '[]']) {
      localStorage.setItem(k, raw);
      expect(loadLayout(k)).toEqual(EMPTY_LAYOUT);
    }
  });

  it('drops foreign elements and clamps or discards bad widths', () => {
    const k = layoutStorageKey('c', 'p');
    localStorage.setItem(
      k,
      JSON.stringify({
        order: [1, 'a', null, { x: 1 }, 'b'],
        hidden: 'x',
        pinned: ['a', 5, 'x'.repeat(10_000)],
        widths: { a: 'w', b: 90, c: 1e12, d: -5, e: null, f: [1] },
      }),
    );
    const l = loadLayout(k);
    expect(l.order).toEqual(['a', 'b']);
    expect(l.hidden).toEqual([]);
    expect(l.pinned).toEqual(['a']);
    expect(l.widths).toEqual({ b: 90, c: 600, d: 56 });
  });

  it('loads a malicious __proto__ key as plain data without polluting prototypes', () => {
    const k = layoutStorageKey('c', 'p');
    localStorage.setItem(
      k,
      '{"widths":{"__proto__":150,"constructor":99999},"order":["__proto__"],"hidden":[],"pinned":[]}',
    );
    const l = loadLayout(k);
    expect(getWidth(l, '__proto__')).toBe(150);
    expect(getWidth(l, 'constructor')).toBe(600);
    expect(Object.getPrototypeOf(l.widths)).toBe(Object.prototype);
    expect(({} as Record<string, unknown>).toString).toBeTypeOf('function');
  });

  it('caps the number of stored entries', () => {
    const k = layoutStorageKey('c', 'p');
    localStorage.setItem(
      k,
      JSON.stringify({ order: Array.from({ length: 50_000 }, (_, i) => `c${i}`) }),
    );
    expect(loadLayout(k).order.length).toBeLessThanOrEqual(2000);
  });
});
