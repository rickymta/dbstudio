import { describe, expect, it } from 'vitest';
import { isTruncatedValue } from '@/features/data/edit/cellEdit';
import { EMPTY_PENDING, extractKey, type PendingTab } from '@/features/data/edit/pendingChanges';
import { hasTruncatedCell } from '@/features/data/edit/rowOps';
import type { ColumnMeta, ResultEditability, Value } from '@/lib/types';
import { applySavedToModel, gridEditability, toResultChangeSet } from './resultEdit';
import { ResultGridModel } from './results';

const col = (name: string, kind: ColumnMeta['valueKind'] = 'text'): ColumnMeta => ({
  name,
  typeName: 'x',
  valueKind: kind,
});
const int = (v: number): Value => ({ t: 'int', v });
const text = (v: string): Value => ({ t: 'text', v });

// SELECT b AS second, a AS first, gen, 1 AS one FROM t   (key: a, b; gen is generated)
const columns = [col('second', 'int'), col('first', 'int'), col('gen'), col('one')];
const re: ResultEditability = {
  editable: true,
  table: { segments: [{ kind: 'table', name: 't' }] },
  keyColumns: ['a', 'b'],
  columnMap: ['b', 'a', 'gen', null],
  readOnlyColumns: ['gen'],
  reason: null,
  atomic: true,
};

describe('gridEditability', () => {
  it('speaks in result column names, keys in the table key order', () => {
    const { editability, hints } = gridEditability(re, columns);
    expect(editability.keyColumns).toEqual(['first', 'second']);
    expect(editability.readOnlyColumns).toEqual(['gen', 'one']);
    expect(hints).toEqual({ one: expect.stringMatching(/expression/i) });
  });
});

describe('toResultChangeSet', () => {
  it('renames cells to table columns and keeps the key aligned with the table key', () => {
    const pending: PendingTab = {
      updates: {
        k: { key: [int(1), int(2)], cells: { second: { kind: 'text', v: '9' } } },
      },
      deletes: {},
      inserts: [{ id: 1, cells: { first: { kind: 'text', v: '5' }, second: { kind: 'default' } } }],
    };
    const cs = toResultChangeSet(re, columns, pending);
    expect(cs).toEqual({
      model: 'rows',
      table: re.table,
      keyColumns: ['a', 'b'],
      changes: [
        {
          op: 'update',
          key: [int(1), int(2)],
          set: [{ column: 'b', value: { kind: 'text', v: '9' } }],
        },
        {
          op: 'insert',
          values: [
            { column: 'a', value: { kind: 'text', v: '5' } },
            { column: 'b', value: { kind: 'default' } },
          ],
        },
      ],
    });
  });
});

describe('applySavedToModel', () => {
  function model() {
    const m = new ResultGridModel('s');
    m.setColumns([col('id', 'int'), col('name')]);
    m.pushRows([
      [int(1), text('a')],
      [int(2), text('b')],
      [int(3), text('c')],
    ]);
    m.finish(false);
    return m;
  }
  const ed = {
    editable: true,
    keyColumns: ['id'],
    readOnlyColumns: [],
    reason: null,
    atomic: true,
  };

  it('updates cells in place and drops deleted rows', () => {
    const m = model();
    const pending: PendingTab = {
      updates: {
        [JSON.stringify([int(1)])]: { key: [int(1)], cells: { name: { kind: 'text', v: 'A' } } },
      },
      deletes: { [JSON.stringify([int(3)])]: [int(3)] },
      inserts: [],
    };
    expect(applySavedToModel(m, ed, pending)).toBe(false);
    expect(m.rows).toEqual([
      [int(1), text('A')],
      [int(2), text('b')],
    ]);
  });

  it('asks for a re-run when the new value or the new row cannot be shown', () => {
    const m = model();
    const withDefault: PendingTab = {
      ...EMPTY_PENDING,
      updates: {
        [JSON.stringify([int(2)])]: { key: [int(2)], cells: { name: { kind: 'default' } } },
      },
    };
    expect(applySavedToModel(m, ed, withDefault)).toBe(true);
    expect(m.rows[1]).toEqual([int(2), text('b')]);
    expect(applySavedToModel(m, ed, { ...EMPTY_PENDING, inserts: [{ id: 1, cells: {} }] })).toBe(
      true,
    );
    expect(m.rows).toHaveLength(3);
  });
});

describe('rows that must not be written back', () => {
  it('a NULL key part (outer side of a join) addresses no row', () => {
    const cols = [col('id', 'int'), col('name')];
    expect(extractKey([int(1), text('a')], cols, ['id'])).toEqual([int(1)]);
    expect(extractKey([{ t: 'null' }, text('a')], cols, ['id'])).toBeNull();
  });

  it('a LOB the server cut short cannot be duplicated or edited', () => {
    const cut: Value = { t: 'ext', v: { type: 'truncatedText', text: 'abc' } };
    expect(isTruncatedValue(cut)).toBe(true);
    expect(isTruncatedValue(text('abc'))).toBe(false);
    expect(hasTruncatedCell([int(1), cut])).toBe(true);
    expect(hasTruncatedCell([int(1), text('a')])).toBe(false);
  });
});
