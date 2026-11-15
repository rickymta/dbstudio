import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Spinner } from '@/components/ui/Spinner';
import { ipc } from '@/lib/ipc';
import type { IpcError } from '@/lib/types';
import { applyTarget, type EditTarget } from './editActions';
import { useEditSession } from './editSession';
import { usePending, type PendingTab } from './pendingChanges';

interface ChangesPreviewDialogProps {
  tabId: string;
  target: EditTarget;
  onClose: () => void;
}

/** The statements of the pending changes, in the order they run, with Apply. */
export function ChangesPreviewDialog({ tabId, target, onClose }: ChangesPreviewDialogProps) {
  const { editability, sessionId } = target;
  const { t } = useTranslation('data');
  const pending = usePending(tabId);
  const session = useEditSession(tabId);
  // Result of the preview request, tagged with the pending state it was made for.
  const [result, setResult] = useState<{
    forPending: PendingTab;
    statements: string[] | null;
    error: IpcError | null;
  } | null>(null);
  const [applying, setApplying] = useState(false);

  useEffect(() => {
    let live = true;
    ipc
      .changesPreview(sessionId, target.changeSet(pending))
      .then((statements) => live && setResult({ forPending: pending, statements, error: null }))
      .catch(
        (error: IpcError) => live && setResult({ forPending: pending, statements: null, error }),
      );
    return () => {
      live = false;
    };
  }, [sessionId, target, pending]);

  const current = result?.forPending === pending ? result : null;
  const statements = current?.statements ?? null;
  const error = current?.error ?? null;

  // Updates run one by one: swapping key values between rows can hit a unique constraint.
  const keyChanged = Object.values(pending.updates).some((u) =>
    editability.keyColumns.some((k) => k in u.cells),
  );
  const failure = session.failure;
  const apply = async () => {
    setApplying(true);
    const ok = await applyTarget(tabId, target);
    setApplying(false);
    if (ok) onClose();
  };

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent
        title={t('edit.previewDialog.title')}
        description={t('edit.previewDialog.description')}
        className="w-[min(760px,94vw)]"
        footer={
          <>
            <Button onClick={onClose}>{t('edit.previewDialog.close')}</Button>
            <Button
              variant="primary"
              onClick={() => void apply()}
              disabled={applying || !statements || statements.length === 0}
            >
              {applying ? t('edit.applying') : t('edit.apply')}
            </Button>
          </>
        }
      >
        {error ? (
          <div role="alert" className="text-xs text-danger">
            <p className="font-medium">{t('edit.previewDialog.errorTitle')}</p>
            <p>{error.message}</p>
          </div>
        ) : !statements ? (
          <div className="flex items-center gap-2 text-xs text-muted-foreground">
            <Spinner /> {t('edit.previewDialog.loading')}
          </div>
        ) : statements.length === 0 ? (
          <p className="text-xs text-muted-foreground">{t('edit.previewDialog.empty')}</p>
        ) : (
          <ol className="flex flex-col gap-2" data-testid="preview-statements">
            {statements.map((sql, i) => {
              const failed = failure?.index === i;
              return (
                <li
                  key={i}
                  data-failed={failed || undefined}
                  className={
                    failed
                      ? 'rounded border border-danger bg-danger-soft p-2'
                      : 'rounded border border-border bg-surface-muted p-2'
                  }
                >
                  <div className="mb-1 text-[11px] text-muted-foreground">
                    {t('edit.previewDialog.statement', { n: i + 1 })}
                  </div>
                  <pre className="font-mono text-xs break-words whitespace-pre-wrap">{sql}</pre>
                  {failed && failure && (
                    <p role="alert" className="mt-1 text-xs text-danger">
                      {t('edit.previewDialog.failedAt', { n: i + 1, message: failure.message })}
                    </p>
                  )}
                </li>
              );
            })}
          </ol>
        )}
        {keyChanged && (
          <p className="mt-2 text-xs text-muted-foreground">
            {t('edit.previewDialog.keyOrderNote')}
          </p>
        )}
        {!editability.atomic && <p className="mt-2 text-xs text-warning">{t('edit.nonAtomic')}</p>}
      </DialogContent>
    </Dialog>
  );
}
