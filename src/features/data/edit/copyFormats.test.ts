import { describe, expect, it } from 'vitest';
import type { ColumnMeta, Value } from '@/lib/types';
import { formatRows, rowsToInsertChangeSet } from './copyFormats';

const cols: ColumnMeta[] = [
  { name: 'id', typeName: 'bigint', valueKind: 'int' },
  { name: 'note', typeName: 'text', valueKind: 'text' },
  { name: 'doc', typeName: 'jsonb', valueKind: 'json' },
];
const rows: Value[][] = [
  [
    { t: 'int', v: '9007199254740993' },
    { t: 'text', v: 'a,"b"\nc' },
    { t: 'json', v: '{"x":1}' },
  ],
  [{ t: 'int', v: 2 }, { t: 'null' }, { t: 'null' }],
];

describe('copy formats', () => {
  it('writes TSV with a header; NULL is empty', () => {
    expect(formatRows('tsv', cols, rows)).toBe(
      'id\tnote\tdoc\n9007199254740993\t"a,""b""\nc"\t"{""x"":1}"\n2\t\t',
    );
  });

  it('quotes CSV fields that need it', () => {
    expect(formatRows('csv', cols, rows)).toBe(
      'id,note,doc\n9007199254740993,"a,""b""\nc","{""x"":1}"\n2,,',
    );
  });

  it('writes JSON objects keeping big integers exact and NULL as null', () => {
    const parsed = JSON.parse(formatRows('json', cols, rows)) as Record<string, unknown>[];
    expect(parsed[0]).toEqual({ id: '9007199254740993', note: 'a,"b"\nc', doc: { x: 1 } });
    expect(parsed[1]).toEqual({ id: 2, note: null, doc: null });
  });

  it('writes Markdown with escaped pipes', () => {
    const md = formatRows('markdown', cols, [
      [{ t: 'int', v: 1 }, { t: 'text', v: 'a|b' }, { t: 'null' }],
    ]);
    expect(md).toBe('| id | note | doc |\n| --- | --- | --- |\n| 1 | a\\|b |  |');
  });

  it('writes an IN list: numbers bare, text quoted, NULL skipped', () => {
    expect(formatRows('in', cols, rows, 0)).toBe('(9007199254740993, 2)');
    expect(formatRows('in', cols, [[{ t: 'text', v: "o'k" }], [{ t: 'null' }]], 0)).toBe(
      "('o''k')",
    );
  });

  it('builds an insert-only change set for the server to render', () => {
    const cs = rowsToInsertChangeSet({ segments: [] }, cols, rows, ['doc']);
    expect(cs).toMatchObject({ model: 'rows', keyColumns: [] });
    if (cs.model !== 'rows') throw new Error('rows expected');
    expect(cs.changes).toHaveLength(2);
    expect(cs.changes[1]).toEqual({
      op: 'insert',
      values: [
        { column: 'id', value: { kind: 'value', v: { t: 'int', v: 2 } } },
        { column: 'note', value: { kind: 'value', v: { t: 'null' } } },
      ],
    });
  });
});
