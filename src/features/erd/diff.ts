// Change detection between a saved layout and the current schema.

import { type LayoutTable, type Snapshot, type ViewTable, tableKey } from './model';
import type { TableModel } from '@/lib/types';

/** `{ tableKey: [[column, typeName], ...] }` of the tables (all columns, not only the shown). */
export function makeSnapshot(tables: readonly TableModel[]): Snapshot {
  const out: Snapshot = {};
  for (const t of tables) {
    out[tableKey(t.path)] = t.columns.map((c) => [c.name, c.typeName]);
  }
  return out;
}

export interface LayoutDiff {
  /** Tables of the layout that no longer exist. */
  missing: LayoutTable[];
  /** Keys of tables that exist but are not in the layout. */
  added: string[];
  /** Keys of tables whose columns or types differ from the snapshot. */
  changed: string[];
}

const same = (a: [string, string][], b: [string, string][]) =>
  a.length === b.length && a.every((c, i) => c[0] === b[i]?.[0] && c[1] === b[i]?.[1]);

export function diffLayout(
  layoutTables: readonly LayoutTable[],
  snapshot: Snapshot,
  current: readonly ViewTable[],
): LayoutDiff {
  const now = new Map(current.map((t) => [t.key, t]));
  const inLayout = new Set(layoutTables.map((t) => tableKey(t.path)));
  const missing = layoutTables.filter((t) => !now.has(tableKey(t.path)));
  const added = current.filter((t) => !inLayout.has(t.key)).map((t) => t.key);
  const changed: string[] = [];
  for (const t of current) {
    const before = snapshot[t.key];
    if (!before) continue;
    const cols = t.table.columns.map<[string, string]>((c) => [c.name, c.typeName]);
    if (!same(before, cols)) changed.push(t.key);
  }
  return { missing, added, changed };
}
