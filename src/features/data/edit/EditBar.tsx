import {
  ClipboardCopy,
  Copy,
  Eye,
  Pencil,
  Plus,
  Trash2,
  Undo2,
  Check,
  RotateCcw,
} from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from '@/components/ui/DropdownMenu';
import { toast } from '@/components/ui/toastStore';
import { cn } from '@/lib/cn';
import { ipc } from '@/lib/ipc';
import type { ObjectPath, Uuid } from '@/lib/types';
import type { GridModel } from '../gridModel';
import { useTabUi } from '../store';
import { useGridVersion } from '../useGridVersion';
import { ChangesPreviewDialog } from './ChangesPreviewDialog';
import { COPY_FORMATS, formatRows, rowsToInsertChangeSet, type CopyFormat } from './copyFormats';
import { applyPending, loadEditability } from './editActions';
import { patchEditSession, useEditSession } from './editSession';
import {
  countChanges,
  extractKey,
  rowKeyString,
  usePending,
  usePendingChanges,
} from './pendingChanges';
import { duplicateCells } from './rowOps';

/** Copying more rows than this at once is refused (the clipboard text would be huge). */
const MAX_COPY_ROWS = 50_000;

interface EditBarProps {
  tabId: string;
  sessionId: Uuid;
  path: ObjectPath;
  model: GridModel;
}

/**
 * Toolbar of the data tab: edit-mode switch (or the reason the table is read-only), row
 * operations, pending change count with Preview / Apply / Discard, and "Copy as".
 */
export function EditBar({ tabId, sessionId, path, model }: EditBarProps) {
  const { t } = useTranslation('data');
  useGridVersion(model);
  const session = useEditSession(tabId);
  const pending = usePending(tabId);
  const [ui, patchUi] = useTabUi(tabId);
  const [previewOpen, setPreviewOpen] = useState(false);
  const [applying, setApplying] = useState(false);

  const loaded = session.editability !== null || session.error !== null;
  useEffect(() => {
    if (!loaded) void loadEditability(tabId, sessionId, path);
  }, [loaded, tabId, sessionId, path]);

  const ed = session.editability;
  const count = countChanges(pending);
  const editMode = session.editMode && ed?.editable === true;
  const store = usePendingChanges.getState;
  const selRow = ui.selection?.row ?? null;
  // New rows come first in the grid, then the loaded rows.
  const insertCount = pending.inserts.length;
  const selInsert = selRow !== null && selRow < insertCount ? pending.inserts[selRow] : undefined;
  const selLoaded =
    selRow !== null && selRow >= insertCount ? (model.rows[selRow - insertCount] ?? null) : null;
  const selKey = selLoaded && ed ? extractKey(selLoaded, model.columns, ed.keyColumns) : null;
  const selDeleted = selKey ? rowKeyString(selKey) in pending.deletes : false;

  const addRow = () => {
    store().addInsert(tabId, {});
    // The new row is row 0; point the selection at it.
    patchUi({ selection: { row: 0, col: ui.selection?.col ?? 0 } });
    patchEditSession(tabId, { failure: null });
  };

  const duplicateRow = () => {
    if (!ed) return;
    let cells;
    if (selLoaded) {
      cells = duplicateCells(selLoaded, model.columns, ed.keyColumns, ed.readOnlyColumns);
      const update = selKey ? pending.updates[rowKeyString(selKey)] : undefined;
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

  const discard = () => {
    if (count > 0 && !window.confirm(t('edit.discardConfirm'))) return;
    store().discard(tabId);
    patchEditSession(tabId, { editing: null, failure: null });
  };

  const apply = async () => {
    if (!ed) return;
    setApplying(true);
    await applyPending({ tabId, sessionId, path, editability: ed, model });
    setApplying(false);
  };

  const copyAs = async (format: CopyFormat, scope: 'row' | 'all') => {
    const rows = scope === 'row' ? (selLoaded ? [selLoaded] : []) : model.rows;
    if (rows.length === 0) {
      toast.info(t('edit.copy.noRow'));
      return;
    }
    if (rows.length > MAX_COPY_ROWS) {
      toast.error(t('edit.copy.tooMany', { count: rows.length }));
      return;
    }
    try {
      const text =
        format === 'insert'
          ? (
              await ipc.changesPreview(
                sessionId,
                rowsToInsertChangeSet(path, model.columns, rows, ed?.readOnlyColumns ?? []),
              )
            ).join('\n')
          : formatRows(format, model.columns, rows, ui.selection?.col ?? 0);
      await navigator.clipboard.writeText(text);
      toast.success(t('edit.copy.done'));
    } catch (e) {
      toast.error(t('edit.copy.error', { message: (e as { message?: string }).message ?? '' }));
    }
  };

  const showActions = editMode || count > 0;
  if (!loaded && model.columns.length === 0) return null;

  return (
    <div className="shrink-0 border-b border-border bg-surface text-xs" data-testid="edit-bar">
      <div className="flex flex-wrap items-center gap-1.5 px-2 py-1">
        {ed?.editable ? (
          <Button
            size="sm"
            aria-pressed={editMode}
            variant={editMode ? 'primary' : 'outline'}
            onClick={() => patchEditSession(tabId, { editMode: !editMode, editing: null })}
          >
            <Pencil size={12} /> {t('edit.toggle')}
          </Button>
        ) : ed ? (
          <span
            role="status"
            data-testid="edit-readonly-reason"
            data-reason={ed.reason ?? undefined}
            className="min-w-0 truncate text-muted-foreground"
            title={t('edit.readOnly', { reason: t(`edit.reason.${ed.reason ?? 'unsupported'}`) })}
          >
            {t('edit.readOnly', { reason: t(`edit.reason.${ed.reason ?? 'unsupported'}`) })}
          </span>
        ) : session.error ? (
          <span role="alert" className="text-danger">
            {t('edit.editabilityError', { message: session.error.message })}
          </span>
        ) : null}

        {editMode && ed && (
          <>
            <Button size="sm" onClick={addRow} disabled={session.applying}>
              <Plus size={12} /> {t('edit.addRow')}
            </Button>
            <Button
              size="sm"
              onClick={duplicateRow}
              disabled={session.applying || (!selLoaded && !selInsert)}
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
          </>
        )}

        {showActions && ed && (
          <>
            <span
              className={cn(
                'px-1',
                count > 0 ? 'font-medium text-warning' : 'text-muted-foreground',
              )}
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
            <Button
              size="sm"
              variant="primary"
              disabled={count === 0 || applying}
              onClick={() => void apply()}
            >
              <Check size={12} /> {applying ? t('edit.applying') : t('edit.apply')}
            </Button>
            <Button size="sm" disabled={count === 0} onClick={discard}>
              <RotateCcw size={12} /> {t('edit.discard')}
            </Button>
          </>
        )}

        <div className="ml-auto">
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button size="sm" disabled={model.columns.length === 0}>
                <ClipboardCopy size={12} /> {t('edit.copy.row')}…
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              {(['row', 'all'] as const).map((scope) => (
                <DropdownMenuSub key={scope}>
                  <DropdownMenuSubTrigger>
                    {scope === 'row' ? t('edit.copy.row') : t('edit.copy.all')}
                  </DropdownMenuSubTrigger>
                  <DropdownMenuSubContent>
                    {COPY_FORMATS.map((f) => (
                      <DropdownMenuItem key={f} onSelect={() => void copyAs(f, scope)}>
                        {t(`edit.copy.format.${f}`)}
                      </DropdownMenuItem>
                    ))}
                  </DropdownMenuSubContent>
                </DropdownMenuSub>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </div>
      {session.failure && (
        <div role="alert" className="bg-danger-soft px-3 py-1 text-danger">
          {t('edit.failure', { n: session.failure.index + 1, message: session.failure.message })}
        </div>
      )}
      {previewOpen && ed && (
        <ChangesPreviewDialog
          tabId={tabId}
          sessionId={sessionId}
          path={path}
          editability={ed}
          model={model}
          onClose={() => setPreviewOpen(false)}
        />
      )}
    </div>
  );
}
