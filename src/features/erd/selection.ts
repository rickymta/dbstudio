// "Table X + N levels": the tables reachable from a root through at most N relations.

import { tableKey } from './model';
import type { Relation } from '@/lib/types';

/** Keys of `rootKey` and every table within `depth` relations of it (either direction). */
export function neighborhood(
  relations: readonly Relation[],
  rootKey: string,
  depth: number,
): Set<string> {
  const adjacent = new Map<string, Set<string>>();
  const link = (a: string, b: string) => {
    const set = adjacent.get(a) ?? new Set<string>();
    set.add(b);
    adjacent.set(a, set);
  };
  for (const r of relations) {
    const a = tableKey(r.fromTable);
    const b = tableKey(r.toTable);
    link(a, b);
    link(b, a);
  }
  const seen = new Set<string>([rootKey]);
  let frontier = [rootKey];
  for (let level = 0; level < depth && frontier.length > 0; level++) {
    const next: string[] = [];
    for (const key of frontier) {
      for (const n of adjacent.get(key) ?? []) {
        if (!seen.has(n)) {
          seen.add(n);
          next.push(n);
        }
      }
    }
    frontier = next;
  }
  return seen;
}

/** Case-insensitive substring match of table names (`schema.name` too). */
export function matchTables<T extends { key: string; label: string }>(
  tables: readonly T[],
  query: string,
): T[] {
  const q = query.trim().toLowerCase();
  if (!q) return [];
  return tables.filter((t) => t.label.toLowerCase().includes(q));
}
