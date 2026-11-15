import { ClipboardCopy, Pencil } from 'lucide-react';
import { useEffect, useMemo } from 'react';
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
import { ipc } from '@/lib/ipc';
import type { ObjectPath, Uuid } from '@/lib/types';
import type { GridModel } from '../gridModel';
import { useTabUi } from '../store';
import { useGridVersion } from '../useGridVersion';
import { COPY_FORMATS, formatRows, rowsToInsertChangeSet, type CopyFormat } from './copyFormats';
import { loadEditability, tableTarget } from './editActions';
import { patchEditSession, useEditSession } from './editSession';
import { EditFailure, EditToolbar } from './EditToolbar';
import { usePending } from './pendingChanges';

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
  usePending(tabId);
  const [ui] = useTabUi(tabId);

  const loaded = session.editability !== null || session.error !== null;
  useEffect(() => {
    if (!loaded) void loadEditability(tabId, sessionId, path);
  }, [loaded, tabId, sessionId, path]);

  const ed = session.editability;
  const editMode = session.editMode && ed?.editable === true;
  const selRow = ui.selection?.row ?? null;
  const insertCount = usePending(tabId).inserts.length;
  const selLoaded =
    selRow !== null && selRow >= insertCount ? (model.rows[selRow - insertCount] ?? null) : null;
  const target = useMemo(
    () => (ed ? tableTarget({ sessionId, path, editability: ed, model }) : null),
    [ed, sessionId, path, model],
  );

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

        {target && <EditToolbar tabId={tabId} model={model} target={target} />}

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
      <EditFailure tabId={tabId} />
    </div>
  );
}
