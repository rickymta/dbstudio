// Pending (uncommitted) row changes of a data tab. Updates and deletes are keyed by the row's
// key values, so they survive a reload/sort/filter of the grid; inserts are new rows shown after
// the loaded ones. Nothing here talks to the database: `buildChanges` turns the state into the
// `RowChange` list that `changes_preview` / `changes_apply` take.

import { create } from 'zustand';
import type { ColumnMeta, EditValue, RowChange, Value } from '@/lib/types';

export interface PendingUpdate {
  key: Value[];
  /** Column name -> new value. */
  cells: Record<string, EditValue>;
}

export interface PendingInsert {
  id: number;
  /** Only the columns the user filled in; the others get the column default. */
  cells: Record<string, EditValue>;
}

export interface PendingTab {
  updates: Record<string, PendingUpdate>;
  deletes: Record<string, Value[]>;
  inserts: PendingInsert[];
}

export const EMPTY_PENDING: PendingTab = { updates: {}, deletes: {}, inserts: [] };

/** Identity of a row: its key values as JSON. */
export function rowKeyString(key: Value[]): string {
  return JSON.stringify(key);
}

/** Key values of `row`, or `null` when a key column is missing from the result. */
export function extractKey(
  row: readonly Value[],
  columns: readonly ColumnMeta[],
  keyColumns: readonly string[],
): Value[] | null {
  if (keyColumns.length === 0) return null;
  const key: Value[] = [];
  for (const name of keyColumns) {
    const i = columns.findIndex((c) => c.name === name);
    const v = i >= 0 ? row[i] : undefined;
    // A NULL key part (the outer side of a join) addresses no row.
    if (!v || v.t === 'null') return null;
    key.push(v);
  }
  return key;
}

export function countChanges(p: PendingTab): number {
  const updates = Object.keys(p.updates).filter((k) => !(k in p.deletes)).length;
  return Object.keys(p.deletes).length + updates + p.inserts.length;
}

/**
 * The change list in the order the server applies it: deletes, then updates, then inserts. The
 * `index` of a `changeFailed` error is a position in this list.
 */
export function buildChanges(p: PendingTab): RowChange[] {
  const out: RowChange[] = [];
  for (const key of Object.values(p.deletes)) out.push({ op: 'delete', key });
  for (const [k, u] of Object.entries(p.updates)) {
    if (k in p.deletes) continue; // the row goes away anyway
    const set = Object.entries(u.cells).map(([column, value]) => ({ column, value }));
    if (set.length > 0) out.push({ op: 'update', key: u.key, set });
  }
  for (const ins of p.inserts) {
    out.push({
      op: 'insert',
      values: Object.entries(ins.cells).map(([column, value]) => ({ column, value })),
    });
  }
  return out;
}

/** What each entry of `buildChanges` refers to, in the same order (maps `changeFailed.index`). */
export type ChangeTarget =
  { kind: 'delete' | 'update'; rowKey: string } | { kind: 'insert'; id: number };

export function changeTargets(p: PendingTab): ChangeTarget[] {
  const out: ChangeTarget[] = Object.keys(p.deletes).map((rowKey) => ({ kind: 'delete', rowKey }));
  for (const [rowKey, u] of Object.entries(p.updates)) {
    if (rowKey in p.deletes || Object.keys(u.cells).length === 0) continue;
    out.push({ kind: 'update', rowKey });
  }
  for (const ins of p.inserts) out.push({ kind: 'insert', id: ins.id });
  return out;
}

interface PendingStore {
  byTab: Record<string, PendingTab>;
  nextInsertId: number;
  /** Set a cell of an existing row; `null` reverts it. */
  setUpdateCell: (tabId: string, key: Value[], column: string, edit: EditValue | null) => void;
  /** Mark or unmark a row for deletion. */
  toggleDelete: (tabId: string, key: Value[]) => void;
  addInsert: (tabId: string, cells: Record<string, EditValue>) => number;
  setInsertCell: (tabId: string, id: number, column: string, edit: EditValue | null) => void;
  removeInsert: (tabId: string, id: number) => void;
  /** Undo the pending change of one existing row (its edited cells and its delete mark). */
  revertRow: (tabId: string, key: Value[]) => void;
  /** Drop every pending change of the tab. */
  discard: (tabId: string) => void;
}

function tabOf(s: PendingStore, tabId: string): PendingTab {
  return s.byTab[tabId] ?? EMPTY_PENDING;
}

function put(s: PendingStore, tabId: string, next: PendingTab): Partial<PendingStore> {
  const byTab = { ...s.byTab };
  const empty =
    Object.keys(next.updates).length === 0 &&
    Object.keys(next.deletes).length === 0 &&
    next.inserts.length === 0;
  if (empty) delete byTab[tabId];
  else byTab[tabId] = next;
  return { byTab };
}

function withCell(
  cells: Record<string, EditValue>,
  column: string,
  edit: EditValue | null,
): Record<string, EditValue> {
  const next = { ...cells };
  if (edit) next[column] = edit;
  else delete next[column];
  return next;
}

export const usePendingChanges = create<PendingStore>((set) => ({
  byTab: {},
  nextInsertId: 1,
  setUpdateCell: (tabId, key, column, edit) =>
    set((s) => {
      const p = tabOf(s, tabId);
      const k = rowKeyString(key);
      const cells = withCell(p.updates[k]?.cells ?? {}, column, edit);
      const updates = { ...p.updates };
      if (Object.keys(cells).length === 0) delete updates[k];
      else updates[k] = { key, cells };
      return put(s, tabId, { ...p, updates });
    }),
  toggleDelete: (tabId, key) =>
    set((s) => {
      const p = tabOf(s, tabId);
      const k = rowKeyString(key);
      const deletes = { ...p.deletes };
      if (k in deletes) delete deletes[k];
      else deletes[k] = key;
      return put(s, tabId, { ...p, deletes });
    }),
  addInsert: (tabId, cells) => {
    let id = 0;
    set((s) => {
      id = s.nextInsertId;
      const p = tabOf(s, tabId);
      return {
        ...put(s, tabId, { ...p, inserts: [...p.inserts, { id, cells }] }),
        nextInsertId: id + 1,
      };
    });
    return id;
  },
  setInsertCell: (tabId, id, column, edit) =>
    set((s) => {
      const p = tabOf(s, tabId);
      const inserts = p.inserts.map((i) =>
        i.id === id ? { ...i, cells: withCell(i.cells, column, edit) } : i,
      );
      return put(s, tabId, { ...p, inserts });
    }),
  removeInsert: (tabId, id) =>
    set((s) => {
      const p = tabOf(s, tabId);
      return put(s, tabId, { ...p, inserts: p.inserts.filter((i) => i.id !== id) });
    }),
  revertRow: (tabId, key) =>
    set((s) => {
      const p = tabOf(s, tabId);
      const k = rowKeyString(key);
      const updates = { ...p.updates };
      const deletes = { ...p.deletes };
      delete updates[k];
      delete deletes[k];
      return put(s, tabId, { ...p, updates, deletes });
    }),
  discard: (tabId) =>
    set((s) => {
      if (!(tabId in s.byTab)) return s;
      const byTab = { ...s.byTab };
      delete byTab[tabId];
      return { byTab };
    }),
}));

/** Pending state of a tab (a stable empty object when there is none). */
export function usePending(tabId: string): PendingTab {
  return usePendingChanges((s) => s.byTab[tabId]) ?? EMPTY_PENDING;
}
