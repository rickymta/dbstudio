import { Copy, Eye, Plus, Trash2, Undo2 } from 'lucide-react';
import { useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { cn } from '@/lib/cn';
import type { GridModel } from '../gridModel';
import { useTabUi } from '../store';
import { useGridVersion } from '../useGridVersion';
import { ChangesPreviewDialog } from './ChangesPreviewDialog';
import type { EditTarget } from './editActions';
import { patchEditSession, useEditSession } from './editSession';
import {
  countChanges,
  extractKey,
  rowKeyString,
  usePending,
  usePendingChanges,
} from './pendingChanges';
import { duplicateCells, hasTruncatedCell } from './rowOps';

interface EditToolbarProps {
  tabId: string;
  model: GridModel;
  target: EditTarget;
  /** Extra controls placed after the standard ones. */
  children?: ReactNode;
}

/**
 * Row operations (add, duplicate, delete, undo) and the pending changes (count, Preview) of an
 * edit session, shared by the table view and the query results. Saving and cancelling are under
 * the grid (`SaveBar`). Only rendered while the grid is in edit mode or has pending changes.
 */
export function EditToolbar({ tabId, model, target, children }: EditToolbarProps) {
  const { t } = useTranslation('data');
  useGridVersion(model);
  const session = useEditSession(tabId);
  const pending = usePending(tabId);
  const [ui, patchUi] = useTabUi(tabId);
  const [previewOpen, setPreviewOpen] = useState(false);

  const ed = target.editability;
  const count = countChanges(pending);
  const editMode = session.editMode && ed.editable;
  const store = usePendingChanges.getState;
  const selRow = ui.selection?.row ?? null;
  // New rows come first in the grid, then the loaded rows.
  const insertCount = pending.inserts.length;
  const selInsert = selRow !== null && selRow < insertCount ? pending.inserts[selRow] : undefined;
  const selLoaded =
    selRow !== null && selRow >= insertCount ? (model.rows[selRow - insertCount] ?? null) : null;
  const selKey = selLoaded ? extractKey(selLoaded, model.columns, ed.keyColumns) : null;
  const selKeyString = selKey ? rowKeyString(selKey) : null;
  const selDeleted = selKeyString ? selKeyString in pending.deletes : false;
  const selChanged = selKeyString ? selKeyString in pending.updates || selDeleted : false;

  const addRow = () => {
    store().addInsert(tabId, {});
    // The new row is row 0; point the selection at it.
    patchUi({ selection: { row: 0, col: ui.selection?.col ?? 0 } });
    patchEditSession(tabId, { failure: null });
  };

  const duplicateRow = () => {
    let cells;
    if (selLoaded) {
      cells = duplicateCells(selLoaded, model.columns, ed.keyColumns, ed.readOnlyColumns);
      const update = selKeyString ? pending.updates[selKeyString] : undefined;
      for (const [col, edit] of Object.entries(update?.cells ?? {})) {
        if (!ed.readOnlyColumns.includes(col) && !ed.keyColumns.includes(col)) cells[col] = edit;
      }
    } else if (selInsert) {
      cells = { ...selInsert.cells };
      for (const k of ed.keyColumns) if (k in cells) cells[k] = { kind: 'default' };
    } else return;
    store().addInsert(tabId, cells);
    patchUi({ selection: { row: 0, col: ui.selection?.col ?? 0 } });
    patchEditSession(tabId, { failure: null });
  };

  const deleteRow = () => {
    if (selInsert) {
      store().removeInsert(tabId, selInsert.id);
      patchUi({ selection: null });
    } else if (selKey) store().toggleDelete(tabId, selKey);
    patchEditSession(tabId, { failure: null });
  };

  const undoRow = () => {
    if (selKey) store().revertRow(tabId, selKey);
    patchEditSession(tabId, { failure: null });
  };

  return (
    <>
      {editMode && (
        <>
          <Button size="sm" onClick={addRow} disabled={session.applying}>
            <Plus size={12} /> {t('edit.addRow')}
          </Button>
          <Button
            size="sm"
            onClick={duplicateRow}
            disabled={
              session.applying ||
              (!selLoaded && !selInsert) ||
              (!!selLoaded && hasTruncatedCell(selLoaded))
            }
            title={selLoaded && hasTruncatedCell(selLoaded) ? t('edit.truncatedCell') : undefined}
          >
            <Copy size={12} /> {t('edit.duplicateRow')}
          </Button>
          <Button
            size="sm"
            onClick={deleteRow}
            disabled={session.applying || (!selKey && !selInsert)}
          >
            {selDeleted ? <Undo2 size={12} /> : <Trash2 size={12} />}
            {selDeleted ? t('edit.undoDelete') : t('edit.deleteRow')}
          </Button>
          <Button size="sm" onClick={undoRow} disabled={session.applying || !selChanged}>
            <Undo2 size={12} /> {t('edit.undoRow')}
          </Button>
        </>
      )}

      {(editMode || count > 0) && (
        <>
          <span
            className={cn('px-1', count > 0 ? 'font-medium text-warning' : 'text-muted-foreground')}
            data-testid="pending-count"
          >
            {count === 0
              ? t('edit.noPending')
              : count === 1
                ? t('edit.pendingOne')
                : t('edit.pending', { count })}
          </span>
          <Button size="sm" disabled={count === 0} onClick={() => setPreviewOpen(true)}>
            <Eye size={12} /> {t('edit.preview')}
          </Button>
        </>
      )}
      {children}
      {previewOpen && (
        <ChangesPreviewDialog tabId={tabId} target={target} onClose={() => setPreviewOpen(false)} />
      )}
    </>
  );
}

/** The banner under the toolbar that points at the change the last apply failed on. */
export function EditFailure({ tabId }: { tabId: string }) {
  const { t } = useTranslation('data');
  const session = useEditSession(tabId);
  if (!session.failure) return null;
  return (
    <div role="alert" className="bg-danger-soft px-3 py-1 text-danger">
      {t('edit.failure', { n: session.failure.index + 1, message: session.failure.message })}
    </div>
  );
}
