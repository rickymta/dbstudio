// Toolbar of an editable query result: edit-mode switch, the shared row operations and pending
// changes of the table view, "Run again", and what applying means for the open transaction.

import { Pencil, RefreshCw } from 'lucide-react';
import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import type { Editability, ResultEditability, Uuid } from '@/lib/types';
import { EditFailure, EditToolbar } from '@/features/data/edit/EditToolbar';
import { patchEditSession, useEditSession } from '@/features/data/edit/editSession';
import { SaveBar } from '@/features/data/edit/SaveBar';
import { lastSegment } from '@/lib/objectPath';
import { resultTarget } from './resultEdit';
import type { ResultGridModel } from './results';

interface ResultEditBarProps {
  gridId: string;
  model: ResultGridModel;
  sessionId: Uuid;
  /** Connection of the editor tab (changes run in its open transaction). */
  leaseId: Uuid | null;
  /** The tab is in manual-commit mode: saved changes wait for Commit. */
  manualCommit: boolean;
  value: ResultEditability;
  editability: Editability;
  needsRerun: boolean;
  /** Run the statement again (asks before dropping unsaved edits); `null`: not possible. */
  onRerun: (() => void) | null;
  /** Changes were saved. */
  onApplied: () => void;
}

export function ResultEditBar({
  gridId,
  model,
  sessionId,
  leaseId,
  manualCommit,
  value,
  editability,
  needsRerun,
  onRerun,
  onApplied,
}: ResultEditBarProps) {
  const { t } = useTranslation('editor');
  const { t: td } = useTranslation('data');
  const session = useEditSession(gridId);
  const editMode = session.editMode && editability.editable;
  const target = useMemo(
    () => resultTarget({ gridId, sessionId, leaseId, value, editability, model, onApplied }),
    [gridId, sessionId, leaseId, value, editability, model, onApplied],
  );
  const table = value.table ? (lastSegment(value.table)?.name ?? '') : '';

  return (
    <div
      className="shrink-0 border-b border-border bg-surface text-xs"
      data-testid="result-edit-bar"
    >
      <div className="flex flex-wrap items-center gap-1.5 px-2 py-1">
        <Button
          size="sm"
          aria-pressed={editMode}
          variant={editMode ? 'primary' : 'outline'}
          title={t('results.edit.table', { table })}
          onClick={() => patchEditSession(gridId, { editMode: !editMode, editing: null })}
        >
          <Pencil size={12} /> {td('edit.toggle')}
        </Button>
        <EditToolbar tabId={gridId} model={model} target={target}>
          <Button size="sm" onClick={() => onRerun?.()} disabled={session.applying || !onRerun}>
            <RefreshCw size={12} /> {t('results.edit.rerun')}
          </Button>
        </EditToolbar>
        {editMode && manualCommit && (
          <span className="text-muted-foreground" data-testid="result-edit-tx-note">
            {t('results.edit.inTransaction')}
          </span>
        )}
      </div>
      <EditFailure tabId={gridId} />
      {needsRerun && (
        <div
          role="status"
          data-testid="result-needs-rerun"
          className="border-t border-border bg-warning-soft px-3 py-1 text-warning"
        >
          {t('results.edit.needsRerun')}
        </div>
      )}
    </div>
  );
}

/** Save / Cancel of the edits of a query result, shown under its grid. */
export function ResultSaveBar({
  gridId,
  model,
  sessionId,
  leaseId,
  value,
  editability,
  onApplied,
}: Pick<
  ResultEditBarProps,
  'gridId' | 'model' | 'sessionId' | 'leaseId' | 'value' | 'editability' | 'onApplied'
>) {
  const { t } = useTranslation('editor');
  const target = useMemo(
    () => resultTarget({ gridId, sessionId, leaseId, value, editability, model, onApplied }),
    [gridId, sessionId, leaseId, value, editability, model, onApplied],
  );
  return (
    <SaveBar tabId={gridId} target={target} discardConfirm={t('results.edit.discardConfirm')} />
  );
}
