import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import type { ConfirmationGrant, ConfirmationRequest } from '@/lib/types';

export interface TypedConfirmDialogProps {
  /** The `confirmation` of a `confirmationRequired` error; `null` closes the dialog. */
  request: ConfirmationRequest | null;
  /** What is about to happen (shown above the input). */
  message: string;
  onConfirm: (grant: ConfirmationGrant) => void;
  onCancel: () => void;
}

/** Asks the user to retype `request.confirmText` before a destructive restore runs. */
export function TypedConfirmDialog({
  request,
  message,
  onConfirm,
  onCancel,
}: TypedConfirmDialogProps) {
  return (
    <Dialog open={request !== null} onOpenChange={(o) => !o && onCancel()}>
      {request && (
        <ConfirmBody
          key={request.token}
          request={request}
          message={message}
          onConfirm={onConfirm}
          onCancel={onCancel}
        />
      )}
    </Dialog>
  );
}

function ConfirmBody({
  request,
  message,
  onConfirm,
  onCancel,
}: Omit<TypedConfirmDialogProps, 'request'> & { request: ConfirmationRequest }) {
  const { t } = useTranslation('backup');
  const [typed, setTyped] = useState('');
  const ok = typed === request.confirmText;
  return (
    <DialogContent
      title={t('confirm.title')}
      className="w-[min(460px,92vw)]"
      footer={
        <>
          <Button onClick={onCancel}>{t('actions.cancel')}</Button>
          <Button
            variant="danger"
            disabled={!ok}
            onClick={() => onConfirm({ token: request.token, typed })}
          >
            {t('confirm.confirm')}
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-3 text-[13px]">
        <p>{message}</p>
        <p>
          {t('confirm.typePrompt')}{' '}
          <code className="rounded bg-surface-muted px-1">{request.confirmText}</code>
        </p>
        <Input
          aria-label={t('confirm.inputLabel')}
          value={typed}
          autoFocus
          spellCheck={false}
          onChange={(e) => setTyped(e.target.value)}
        />
      </div>
    </DialogContent>
  );
}
