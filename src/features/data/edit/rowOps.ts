// Row operations of the edit mode: new and duplicated rows.

import type { ColumnMeta, EditValue, Value } from '@/lib/types';

/**
 * Cells of a duplicate of `row`. Read-only columns are left out (the server fills them) and key
 * columns become DEFAULT: copying a primary key would collide, and a column without a default
 * makes the server report it so the user types a new key.
 */
export function duplicateCells(
  row: readonly Value[],
  columns: readonly ColumnMeta[],
  keyColumns: readonly string[],
  readOnlyColumns: readonly string[],
): Record<string, EditValue> {
  const cells: Record<string, EditValue> = {};
  columns.forEach((c, i) => {
    if (readOnlyColumns.includes(c.name)) return;
    const v = row[i];
    if (!v) return;
    cells[c.name] = keyColumns.includes(c.name)
      ? { kind: 'default' }
      : { kind: 'value', v: insertable(v) };
  });
  return cells;
}

/** Arrays stay typed (the server renders them); documents have no SQL literal: JSON text. */
function insertable(v: Value): Value {
  if (v.t === 'document') return { t: 'json', v: JSON.stringify(plain(v)) };
  return v;
}

function plain(v: Value): unknown {
  switch (v.t) {
    case 'array':
      return v.v.map(plain);
    case 'document':
      return Object.fromEntries(v.v.map(([k, x]) => [k, plain(x)]));
    case 'null':
      return null;
    case 'ext':
      return v.v.text;
    default:
      return v.v;
  }
}
