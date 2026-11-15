// Editing the rows of a query result (the "Edit" mode of the results grid).
//
// The grid and the toolbar of the table view are reused as they are: the edit session of a result
// grid is keyed by the grid id and speaks in RESULT column names. This file maps between the two
// worlds: `ResultEditability` (table columns, `column_map`) becomes the grid's `Editability`,
// pending changes become the change set of the base table, and saved changes are mirrored into
// the loaded rows.

import { create } from 'zustand';
import i18n from '@/lib/i18n';
import { ipc, toIpcError } from '@/lib/ipc';
import type {
  ApplyResult,
  ChangeSet,
  ColumnMeta,
  Editability,
  IpcError,
  ResultEditability,
  ResultNotEditableReason,
  RowChange,
  Uuid,
} from '@/lib/types';
import { useDataUiStore } from '@/features/data/store';
import { editDisplayValue } from '@/features/data/edit/cellEdit';
import type { EditTarget } from '@/features/data/edit/editActions';
import { patchEditSession, useEditSessionStore } from '@/features/data/edit/editSession';
import {
  buildChanges,
  countChanges,
  extractKey,
  rowKeyString,
  usePendingChanges,
  type PendingTab,
} from '@/features/data/edit/pendingChanges';
import type { ResultGridModel } from './results';

/** Reason shown when the result cannot be edited (backend reasons plus one of the grid's own). */
export type ResultReason = ResultNotEditableReason | 'duplicateColumns';

export interface ResultEditState {
  status: 'loading' | 'ready' | 'error';
  value: ResultEditability | null;
  reason: ResultReason | null;
  error: IpcError | null;
  /** Saved changes the loaded rows cannot show (new rows, DEFAULT values): run the query again. */
  needsRerun: boolean;
}

interface ResultEditStore {
  byGrid: Record<string, ResultEditState>;
  patch: (gridId: string, patch: Partial<ResultEditState>) => void;
}

export const useResultEditStore = create<ResultEditStore>((set) => ({
  byGrid: {},
  patch: (gridId, patch) =>
    set((s) => ({
      byGrid: {
        ...s.byGrid,
        [gridId]: {
          status: 'loading',
          value: null,
          reason: null,
          error: null,
          needsRerun: false,
          ...s.byGrid[gridId],
          ...patch,
        },
      },
    })),
}));

export function useResultEdit(gridId: string): ResultEditState | undefined {
  return useResultEditStore((s) => s.byGrid[gridId]);
}

export const patchResultEdit = (gridId: string, patch: Partial<ResultEditState>) =>
  useResultEditStore.getState().patch(gridId, patch);

// -- editability --------------------------------------------------------------------------------

/**
 * The editability the grid works with: key and read-only columns by RESULT column name. Columns
 * that are not a plain table column (expressions, constants) and generated ones are read-only,
 * each with its own tooltip.
 */
export function gridEditability(
  re: ResultEditability,
  columns: readonly ColumnMeta[],
): { editability: Editability; hints: Record<string, string> } {
  const names = columns.map((c) => c.name);
  const keyColumns = re.keyColumns.flatMap((k) => {
    const i = re.columnMap.indexOf(k);
    return i >= 0 ? [names[i]!] : [];
  });
  const readOnlyColumns: string[] = [];
  const hints: Record<string, string> = {};
  columns.forEach((c, i) => {
    const mapped = re.columnMap[i] ?? null;
    if (mapped === null) {
      readOnlyColumns.push(c.name);
      hints[c.name] = c.origin
        ? i18n.t('editor:results.edit.duplicateColumn')
        : i18n.t('editor:results.edit.expressionColumn');
    } else if (re.readOnlyColumns.includes(mapped)) {
      readOnlyColumns.push(c.name);
    }
  });
  return {
    editability: {
      editable: re.editable,
      keyColumns,
      readOnlyColumns,
      reason: null,
      atomic: re.atomic,
      notes: re.notes,
    },
    hints,
  };
}

/** Does a result need an `ipc.resultEditability` call (some column knows its table)? */
export const hasOrigins = (columns: readonly ColumnMeta[]): boolean =>
  columns.some((c) => !!c.origin);

function duplicateNames(columns: readonly ColumnMeta[]): boolean {
  return new Set(columns.map((c) => c.name)).size !== columns.length;
}

/** Ask the backend whether the result can be edited and set up the grid's edit session. */
export async function loadResultEditability(opts: {
  gridId: string;
  sessionId: Uuid;
  leaseId: Uuid | null;
  columns: ColumnMeta[];
}): Promise<void> {
  const { gridId, sessionId, leaseId, columns } = opts;
  const state = useResultEditStore.getState().byGrid[gridId];
  if (state && state.status !== 'error') return;
  patchResultEdit(gridId, { status: 'loading', error: null });
  try {
    const value = await ipc.resultEditability(sessionId, leaseId, columns);
    const reason: ResultReason | null = value.editable
      ? duplicateNames(columns)
        ? 'duplicateColumns'
        : null
      : (value.reason ?? 'unsupported');
    const editable = value.editable && reason === null;
    patchResultEdit(gridId, { status: 'ready', value, reason });
    if (editable) {
      const { editability, hints } = gridEditability(value, columns);
      patchEditSession(gridId, { editability, error: null, readOnlyHints: hints });
    }
  } catch (e) {
    patchResultEdit(gridId, { status: 'error', error: toIpcError(e) });
  }
}

// -- change sets --------------------------------------------------------------------------------

/** Table column of each result column name. */
function tableNames(re: ResultEditability, columns: readonly ColumnMeta[]): Map<string, string> {
  const m = new Map<string, string>();
  columns.forEach((c, i) => {
    const t = re.columnMap[i];
    if (t) m.set(c.name, t);
  });
  return m;
}

/** The change set of the base table for the pending changes of a result grid. */
export function toResultChangeSet(
  re: ResultEditability,
  columns: readonly ColumnMeta[],
  pending: PendingTab,
): ChangeSet {
  const rename = tableNames(re, columns);
  const name = (c: string) => rename.get(c) ?? c;
  const changes: RowChange[] = buildChanges(pending).map((c) => {
    switch (c.op) {
      case 'update':
        return { ...c, set: c.set.map((x) => ({ ...x, column: name(x.column) })) };
      case 'insert':
        return { ...c, values: c.values.map((x) => ({ ...x, column: name(x.column) })) };
      default:
        return c;
    }
  });
  return { model: 'rows', table: re.table!, keyColumns: re.keyColumns, changes };
}

/**
 * Mirror saved changes in the loaded rows: edited cells take the new value, deleted rows go.
 * New rows and DEFAULT values cannot be shown (the server fills them in): they set `needsRerun`.
 */
export function applySavedToModel(
  model: ResultGridModel,
  editability: Editability,
  pending: PendingTab,
): boolean {
  const { columns } = model;
  let needsRerun = pending.inserts.length > 0;
  const index = new Map<string, number>();
  model.rows.forEach((row, i) => {
    const key = extractKey(row, columns, editability.keyColumns);
    if (key) index.set(rowKeyString(key), i);
  });
  for (const [k, u] of Object.entries(pending.updates)) {
    if (k in pending.deletes) continue;
    const row = model.rows[index.get(k) ?? -1];
    if (!row) continue;
    for (const [column, edit] of Object.entries(u.cells)) {
      const ci = columns.findIndex((c) => c.name === column);
      if (ci < 0) continue;
      const shown = editDisplayValue(edit, columns[ci]!.valueKind);
      if (shown) row[ci] = shown;
      else needsRerun = true;
    }
  }
  const gone = new Set(
    Object.keys(pending.deletes)
      .map((k) => index.get(k))
      .filter((i): i is number => i !== undefined),
  );
  if (gone.size > 0) model.rows = model.rows.filter((_, i) => !gone.has(i));
  model.touch();
  return needsRerun;
}

/**
 * Edit target of a result grid. `leaseId` is the editor tab's connection: with a transaction
 * open on it the backend applies the changes inside that transaction.
 */
export function resultTarget(opts: {
  gridId: string;
  sessionId: Uuid;
  leaseId: Uuid | null;
  value: ResultEditability;
  editability: Editability;
  model: ResultGridModel;
  onApplied?: (applied: ApplyResult) => void;
}): EditTarget {
  const { gridId, sessionId, leaseId, value, editability, model } = opts;
  return {
    sessionId,
    editability,
    leaseId,
    changeSet: (pending) => toResultChangeSet(value, model.columns, pending),
    onApplied: (applied, pending) => {
      const needsRerun = applySavedToModel(model, editability, pending);
      if (needsRerun) patchResultEdit(gridId, { needsRerun: true });
      // Row positions moved (deleted rows): a stale selection would point at another row.
      useDataUiStore.getState().patch(gridId, { selection: null });
      opts.onApplied?.(applied);
    },
  };
}

// -- state of a tab's results ------------------------------------------------------------------

const prefixOf = (tabStateId: string) => `${tabStateId}:`;

/** Does any result of the editor tab have unsaved edits? */
export function hasResultEdits(tabStateId: string): boolean {
  const prefix = prefixOf(tabStateId);
  return Object.entries(usePendingChanges.getState().byTab).some(
    ([id, p]) => id.startsWith(prefix) && countChanges(p) > 0,
  );
}

/** Forget the edit state of every result of the tab (new run, tab closed). */
export function dropResultEdits(tabStateId: string): void {
  const prefix = prefixOf(tabStateId);
  const pending = usePendingChanges.getState();
  for (const id of Object.keys(pending.byTab)) if (id.startsWith(prefix)) pending.discard(id);
  const sessions = useEditSessionStore.getState();
  for (const id of Object.keys(sessions.byTab)) if (id.startsWith(prefix)) sessions.drop(id);
  useResultEditStore.setState((s) => {
    const byGrid = Object.fromEntries(
      Object.entries(s.byGrid).filter(([id]) => !id.startsWith(prefix)),
    );
    return { byGrid };
  });
}

/** Subscribe to changes of the pending edits of results (for the close guard). */
export function onResultEditsChanged(listener: (tabStateId: string) => void): () => void {
  return usePendingChanges.subscribe((state, previous) => {
    const touched = new Set<string>();
    for (const id of new Set([...Object.keys(state.byTab), ...Object.keys(previous.byTab)])) {
      if (state.byTab[id] === previous.byTab[id]) continue;
      // A result grid id is `<tab state id>:<run id>:<result key>`; tab state ids are UUIDs.
      const m = /^(.+):\d+:r\d+$/.exec(id);
      if (m) touched.add(m[1]!);
    }
    for (const id of touched) listener(id);
  });
}

/** Ask whether unsaved result edits may be thrown away (re-run, close). */
export function confirmDiscardResultEdits(message: string): boolean {
  return window.confirm(message);
}
