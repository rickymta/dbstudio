// Flat, first-level columns for a collection, inferred from the documents loaded so far
// (documents are not uniform: later documents may add keys; D7 in the design).

import type { Value } from '@/lib/types';

/** Upper bound of inferred columns; keys beyond it are dropped and `truncated` is set. */
export const MAX_FLAT_COLUMNS = 200;

export type DocFields = [string, Value][];

export interface FlatColumn {
  /** First-level key; also the dotted path used for sort and filter. */
  name: string;
  /** Sorting on it is possible (keys with `.` or a leading `$` cannot be addressed by a path). */
  sortable: boolean;
}

/** A document value's field list, or `null` when the value is not a document. */
export function docFields(v: Value | undefined): DocFields | null {
  return v?.t === 'document' ? v.v : null;
}

/** First value of `key` in a document (`undefined` when the key is absent). */
export function fieldOf(doc: DocFields, key: string): Value | undefined {
  for (const [k, v] of doc) if (k === key) return v;
  return undefined;
}

/** `_id` of a document value, if it has one. */
export function docId(v: Value | undefined): Value | undefined {
  const f = docFields(v);
  return f ? fieldOf(f, '_id') : undefined;
}

/** Dotted path of nested keys (`['address', 'city']` -> `address.city`). */
export function toDotPath(path: readonly string[]): string {
  return path.join('.');
}

/** Split a dotted path back into keys. */
export function fromDotPath(path: string): string[] {
  return path === '' ? [] : path.split('.');
}

/** Value at a nested path of keys; array steps accept numeric indexes. */
export function valueAtPath(v: Value | undefined, path: readonly string[]): Value | undefined {
  let cur = v;
  for (const key of path) {
    if (!cur) return undefined;
    if (cur.t === 'document') cur = fieldOf(cur.v, key);
    else if (cur.t === 'array' && /^\d+$/.test(key)) cur = cur.v[Number(key)];
    else return undefined;
  }
  return cur;
}

export function isSortableKey(name: string): boolean {
  return name !== '' && !name.includes('.') && !name.startsWith('$');
}

/**
 * Incremental column inference. `update` scans only the rows added since the last call (rows are
 * appended while a collection loads); a shorter array means a reload and starts over.
 * `_id` is always the first column; other keys follow in order of first appearance.
 */
export class ColumnTracker {
  readonly max: number;
  private keys: string[] = [];
  private seen = new Set<string>();
  private scanned = 0;
  truncated = false;

  constructor(max = MAX_FLAT_COLUMNS) {
    this.max = max;
  }

  get columns(): FlatColumn[] {
    return this.keys.map((name) => ({ name, sortable: isSortableKey(name) }));
  }

  /** Scan new rows; returns true when the column set changed. */
  update(rows: readonly Value[][]): boolean {
    let changed = false;
    if (rows.length < this.scanned) {
      this.keys = [];
      this.seen = new Set();
      this.scanned = 0;
      this.truncated = false;
      changed = true;
    }
    for (let r = this.scanned; r < rows.length; r++) {
      const fields = docFields(rows[r]?.[0]);
      if (!fields) continue;
      for (const [key] of fields) {
        if (this.seen.has(key)) continue;
        if (this.keys.length >= this.max) {
          this.truncated = true;
          continue;
        }
        this.seen.add(key);
        if (key === '_id') this.keys.unshift(key);
        else this.keys.push(key);
        changed = true;
      }
    }
    this.scanned = rows.length;
    return changed;
  }
}

/** Columns of a set of documents (one-shot form of `ColumnTracker`). */
export function inferColumns(
  docs: readonly Value[],
  max = MAX_FLAT_COLUMNS,
): { columns: FlatColumn[]; truncated: boolean } {
  const tracker = new ColumnTracker(max);
  tracker.update(docs.map((d) => [d]));
  return { columns: tracker.columns, truncated: tracker.truncated };
}

export type CellSummary =
  /** The document has no such key (different from `null`). */
  | { kind: 'missing' }
  | { kind: 'null' }
  | { kind: 'document'; count: number }
  | { kind: 'array'; count: number }
  | { kind: 'scalar'; value: Value };

/** What a table cell shows for `value` (`undefined` = key missing). */
export function summarizeCell(value: Value | undefined): CellSummary {
  if (value === undefined) return { kind: 'missing' };
  switch (value.t) {
    case 'null':
      return { kind: 'null' };
    case 'document':
      return { kind: 'document', count: value.v.length };
    case 'array':
      return { kind: 'array', count: value.v.length };
    default:
      return { kind: 'scalar', value };
  }
}

/** Plain-text form of a nested-value summary (`{3 fields}`, `[5 items]`). */
export function summaryText(s: CellSummary): string | null {
  switch (s.kind) {
    case 'document':
      return s.count === 1 ? '{1 field}' : `{${s.count} fields}`;
    case 'array':
      return s.count === 1 ? '[1 item]' : `[${s.count} items]`;
    default:
      return null;
  }
}
