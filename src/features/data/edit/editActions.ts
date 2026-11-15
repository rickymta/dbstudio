// Side effects of the edit mode: reading editability, applying the pending changes.

import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import type { ApplyResult, ChangeSet, Editability, IpcError, ObjectPath, Uuid } from '@/lib/types';
import { toast } from '@/components/ui/toastStore';
import type { GridModel } from '../gridModel';
import { patchEditSession, useEditSessionStore } from './editSession';
import { buildChanges, usePendingChanges, type PendingTab } from './pendingChanges';

/** The change set sent to `changes_preview` / `changes_apply`. */
export function toChangeSet(
  table: ObjectPath,
  editability: Editability,
  pending: PendingTab,
): ChangeSet {
  return {
    model: 'rows',
    table,
    keyColumns: editability.keyColumns,
    changes: buildChanges(pending),
  };
}

/** Read the table's editability once; failures are kept for the toolbar to show. */
export async function loadEditability(
  tabId: string,
  sessionId: Uuid,
  path: ObjectPath,
): Promise<void> {
  try {
    const editability = await ipc.tableEditability(sessionId, path);
    patchEditSession(tabId, { editability, error: null });
  } catch (e) {
    patchEditSession(tabId, { editability: null, error: e as IpcError });
  }
}

/**
 * What an edit session writes to: the rows of a table view, or of a query result. The grid and
 * the toolbar only know pending changes by the grid's column names; the target turns them into
 * the change set the backend takes and decides what happens to the grid once they are saved.
 */
export interface EditTarget {
  sessionId: Uuid;
  /** Editability in the grid's own column names. */
  editability: Editability;
  /** The change set of `pending` (column names of the database). */
  changeSet: (pending: PendingTab) => ChangeSet;
  /** Connection to run the changes on (an open transaction); the session's when absent. */
  leaseId?: Uuid | null;
  /** The changes were saved: bring the grid up to date. */
  onApplied: (applied: ApplyResult, pending: PendingTab) => void;
}

/** Edit target of a table view: saved changes reload the grid. */
export function tableTarget(opts: {
  sessionId: Uuid;
  path: ObjectPath;
  editability: Editability;
  model: GridModel;
}): EditTarget {
  const { sessionId, path, editability, model } = opts;
  return {
    sessionId,
    editability,
    changeSet: (pending) => toChangeSet(path, editability, pending),
    onApplied: () => model.reload(),
  };
}

/**
 * Apply the pending changes in one transaction. On success the pending changes are dropped and
 * the target updates the grid; on failure nothing changed on the server and the pending changes
 * stay, with the failing change recorded for the grid to point at. Returns whether it succeeded.
 */
export async function applyTarget(tabId: string, target: EditTarget): Promise<boolean> {
  const pending = usePendingChanges.getState().byTab[tabId];
  if (!pending) return true;
  if (useEditSessionStore.getState().byTab[tabId]?.applying) return false;
  const changeSet = target.changeSet(pending);
  if (changeSet.model === 'rows' && changeSet.changes.length === 0) return true;
  // Lock the grid while the request is in flight: an edit made now would be wiped by the
  // success path below without ever having been sent.
  patchEditSession(tabId, { applying: true, editing: null });
  try {
    const r = await ipc.changesApply(target.sessionId, changeSet, target.leaseId ?? null);
    usePendingChanges.getState().discard(tabId);
    patchEditSession(tabId, { editing: null, failure: null });
    toast.success(
      r.applied === 1
        ? i18n.t('data:edit.appliedOne')
        : i18n.t('data:edit.applied', { count: r.applied }),
    );
    target.onApplied(r, pending);
    return true;
  } catch (e) {
    const err = e as IpcError;
    if (err.kind === 'changeFailed' && typeof err.index === 'number') {
      patchEditSession(tabId, { failure: { index: err.index, message: err.message } });
    } else {
      toast.error(i18n.t('data:edit.applyError', { message: err.message }));
    }
    return false;
  } finally {
    patchEditSession(tabId, { applying: false });
  }
}

/** [`applyTarget`] for a table view. */
export function applyPending(opts: {
  tabId: string;
  sessionId: Uuid;
  path: ObjectPath;
  editability: Editability;
  model: GridModel;
}): Promise<boolean> {
  return applyTarget(opts.tabId, tableTarget(opts));
}
