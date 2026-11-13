import { describe, expect, it } from 'vitest';
import type { ObjectPath } from '@/lib/types';
import {
  buildApplied,
  buildCountRequest,
  buildFilters,
  buildTableRequest,
  emptyDraft,
  isFilterActive,
  nextSort,
  parseInList,
  type FilterRowDraft,
} from './request';

const row = (p: Partial<FilterRowDraft>): FilterRowDraft => ({
  id: 'r',
  enabled: true,
  column: 'status',
  op: 'eq',
  value: '',
  ...p,
});

const table: ObjectPath = { segments: [{ kind: 'table', name: 't' }] };

describe('filters', () => {
  it('splits `in` lists on commas and trims blanks', () => {
    expect(parseInList(' a, b ,, c ,')).toEqual(['a', 'b', 'c']);
    expect(parseInList('  ')).toEqual([]);
  });

  it('builds filters from enabled, complete rows only', () => {
    const filters = buildFilters([
      row({ op: 'eq', value: 'paid' }),
      row({ op: 'in', value: '1, 2,3' }),
      row({ op: 'isNull', column: 'message', value: 'ignored' }),
      row({ op: 'isNotNull', column: 'user_id' }),
      row({ op: 'contains', value: 'x', enabled: false }),
      row({ op: 'eq', value: '' }),
      row({ op: 'in', value: ' , ' }),
      row({ op: 'eq', column: '', value: 'x' }),
    ]);
    expect(filters).toEqual([
      { column: 'status', op: 'eq', values: ['paid'] },
      { column: 'status', op: 'in', values: ['1', '2', '3'] },
      { column: 'message', op: 'isNull', values: [] },
      { column: 'user_id', op: 'isNotNull', values: [] },
    ]);
  });

  it('applies either builder rows or a raw WHERE, never both', () => {
    const draft = { ...emptyDraft(), rows: [row({ value: 'a' })], raw: ' id > 5 ' };
    expect(buildApplied({ ...draft, mode: 'builder' })).toEqual({
      filters: [{ column: 'status', op: 'eq', values: ['a'] }],
      rawWhere: null,
    });
    expect(buildApplied({ ...draft, mode: 'raw' })).toEqual({ filters: [], rawWhere: 'id > 5' });
    expect(buildApplied({ ...draft, mode: 'raw', raw: '   ' }).rawWhere).toBeNull();
    expect(isFilterActive(buildApplied({ ...draft, mode: 'raw' }))).toBe(true);
    expect(isFilterActive(buildApplied(emptyDraft()))).toBe(false);
  });
});

describe('nextSort', () => {
  it('cycles asc -> desc -> off on a single column', () => {
    let s = nextSort([], 'id', false);
    expect(s).toEqual([{ column: 'id', descending: false }]);
    s = nextSort(s, 'id', false);
    expect(s).toEqual([{ column: 'id', descending: true }]);
    s = nextSort(s, 'id', false);
    expect(s).toEqual([]);
  });

  it('plain click on another column replaces the sort', () => {
    const s = nextSort([{ column: 'a', descending: true }], 'b', false);
    expect(s).toEqual([{ column: 'b', descending: false }]);
  });

  it('shift click appends and cycles within a multi-column sort', () => {
    let s = nextSort([{ column: 'a', descending: false }], 'b', true);
    expect(s).toEqual([
      { column: 'a', descending: false },
      { column: 'b', descending: false },
    ]);
    s = nextSort(s, 'a', true);
    expect(s).toEqual([
      { column: 'a', descending: true },
      { column: 'b', descending: false },
    ]);
    s = nextSort(s, 'a', true);
    expect(s).toEqual([{ column: 'b', descending: false }]);
  });
});

describe('request building', () => {
  it('builds a table data request from the state', () => {
    const applied = buildApplied({ ...emptyDraft(), mode: 'raw', raw: 'id > 1' });
    const req = buildTableRequest({
      queryId: 'q',
      table,
      applied,
      sort: [{ column: 'id', descending: true }],
      offset: 2000,
      limit: 1000,
      batchSize: 500,
    });
    expect(req).toEqual({
      queryId: 'q',
      table,
      filters: [],
      rawWhere: 'id > 1',
      sort: [{ column: 'id', descending: true }],
      offset: 2000,
      limit: 1000,
      batchSize: 500,
    });
    expect(buildCountRequest(table, applied)).toEqual({ table, filters: [], rawWhere: 'id > 1' });
  });
});
