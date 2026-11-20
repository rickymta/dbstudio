import { useTranslation } from 'react-i18next';
import { Button } from './Button';
import { useConfirmStore } from './confirm';
import { Dialog, DialogContent } from './Dialog';

/** Renders the questions asked with `confirmDialog`, one at a time. Mounted once in the app. */
export function ConfirmHost() {
  const { t } = useTranslation();
  const request = useConfirmStore((s) => s.queue[0]);
  const answer = useConfirmStore((s) => s.answer);
  if (!request) return null;
  return (
    <Dialog
      // A new key per question: the next one in the queue opens as a fresh dialog.
      key={request.id}
      open
      onOpenChange={(open) => !open && answer(request.id, false)}
    >
      <DialogContent
        title={request.title}
        description={request.description}
        className="w-[min(420px,92vw)]"
        footer={
          <>
            <Button onClick={() => answer(request.id, false)}>
              {request.cancelLabel ?? t('actions.cancel')}
            </Button>
            <Button
              autoFocus
              data-testid="confirm-dialog-ok"
              variant={request.destructive ? 'danger' : 'primary'}
              onClick={() => answer(request.id, true)}
            >
              {request.confirmLabel ?? t('actions.confirm')}
            </Button>
          </>
        }
      />
    </Dialog>
  );
}
