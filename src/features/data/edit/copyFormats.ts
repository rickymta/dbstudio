// Copy rows as CSV, TSV, JSON, Markdown, an `IN (...)` list or SQL INSERT (DATA-08). Everything
// is pure except the INSERT format, which asks the server to render the statements
// (`rowsToInsertChangeSet` + `changes_preview`) so quoting follows the database's dialect.

import type { ChangeSet, ColumnMeta, EditValue, ObjectPath, Value } from '@/lib/types';
import { valueToText } from '../format';

export const COPY_FORMATS = ['tsv', 'csv', 'json', 'markdown', 'in', 'insert'] as const;
export type CopyFormat = (typeof COPY_FORMATS)[number];

function csvField(v: Value, delimiter: string): string {
  if (v.t === 'null') return '';
  const text = valueToText(v);
  return /["\r\n]/.test(text) || text.includes(delimiter) ? `"${text.replace(/"/g, '""')}"` : text;
}

function delimited(
  columns: readonly ColumnMeta[],
  rows: readonly (readonly Value[])[],
  delimiter: string,
): string {
  const head = columns.map((c) => csvField({ t: 'text', v: c.name }, delimiter));
  const lines = rows.map((r) => r.map((v) => csvField(v, delimiter)).join(delimiter));
  return [head.join(delimiter), ...lines].join('\n');
}

/** Value as a plain JSON-able value; large integers stay exact as strings. */
function jsonValue(v: Value): unknown {
  switch (v.t) {
    case 'null':
      return null;
    case 'bool':
    case 'text':
    case 'decimal':
    case 'date':
    case 'time':
    case 'dateTime':
    case 'timestamp':
    case 'uuid':
      return v.v;
    case 'int':
    case 'uInt':
    case 'float':
      if (typeof v.v === 'number') return v.v;
      return Number.isSafeInteger(Number(v.v)) && String(Number(v.v)) === v.v ? Number(v.v) : v.v;
    case 'json':
      try {
        return JSON.parse(v.v);
      } catch {
        return v.v;
      }
    case 'array':
      return v.v.map(jsonValue);
    case 'document':
      return Object.fromEntries(v.v.map(([k, x]) => [k, jsonValue(x)]));
    default:
      return valueToText(v);
  }
}

function markdown(columns: readonly ColumnMeta[], rows: readonly (readonly Value[])[]): string {
  const cell = (v: Value) =>
    (v.t === 'null' ? '' : valueToText(v)).replace(/\|/g, '\\|').replace(/\r?\n/g, ' ');
  const head = `| ${columns.map((c) => c.name.replace(/\|/g, '\\|')).join(' | ')} |`;
  const sep = `| ${columns.map(() => '---').join(' | ')} |`;
  return [head, sep, ...rows.map((r) => `| ${r.map(cell).join(' | ')} |`)].join('\n');
}

/** `('a', 'b', 3)` from the first column of every row (numbers unquoted, text quoted). */
function inList(rows: readonly (readonly Value[])[], col: number): string {
  const items = rows.map((r) => r[col]).filter((v): v is Value => !!v && v.t !== 'null');
  const lit = (v: Value) => {
    if (v.t === 'int' || v.t === 'uInt' || v.t === 'decimal' || v.t === 'float') return String(v.v);
    if (v.t === 'bool') return v.v ? 'TRUE' : 'FALSE';
    return `'${valueToText(v).replace(/'/g, "''")}'`;
  };
  return `(${items.map(lit).join(', ')})`;
}

/** Text of every format except `insert`. `inColumn` is the column of the IN list. */
export function formatRows(
  format: Exclude<CopyFormat, 'insert'>,
  columns: readonly ColumnMeta[],
  rows: readonly (readonly Value[])[],
  inColumn = 0,
): string {
  switch (format) {
    case 'tsv':
      return delimited(columns, rows, '\t');
    case 'csv':
      return delimited(columns, rows, ',');
    case 'json':
      return JSON.stringify(
        rows.map((r) => Object.fromEntries(columns.map((c, i) => [c.name, jsonValue(r[i]!)]))),
        null,
        2,
      );
    case 'markdown':
      return markdown(columns, rows);
    case 'in':
      return inList(rows, inColumn);
  }
}

/** A change set of plain inserts of `rows`, for `changes_preview`. */
export function rowsToInsertChangeSet(
  table: ObjectPath,
  columns: readonly ColumnMeta[],
  rows: readonly (readonly Value[])[],
  skipColumns: readonly string[] = [],
): ChangeSet {
  return {
    model: 'rows',
    table,
    keyColumns: [],
    changes: rows.map((r) => ({
      op: 'insert' as const,
      values: columns.flatMap((c, i) => {
        const v = r[i];
        if (!v || skipColumns.includes(c.name)) return [];
        const value: EditValue = { kind: 'value', v };
        return [{ column: c.name, value }];
      }),
    })),
  };
}
