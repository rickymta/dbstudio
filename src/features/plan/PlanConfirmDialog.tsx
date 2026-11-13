import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import type { ConfirmationGrant, ConfirmationRequest } from '@/lib/types';

interface Props {
  /** The `confirmation` of a `confirmationRequired` answer; `null` closes the dialog. */
  request: ConfirmationRequest | null;
  onConfirm: (grant: ConfirmationGrant) => void;
  onCancel: () => void;
}

/** Asks the user to type the confirmation text before an ANALYZE runs a write on production. */
export function PlanConfirmDialog({ request, onConfirm, onCancel }: Props) {
  const { t } = useTranslation('plan');
  const [typed, setTyped] = useState('');
  return (
    <Dialog
      open={request !== null}
      onOpenChange={(open) => {
        if (!open) {
          setTyped('');
          onCancel();
        }
      }}
    >
      {request && (
        <DialogContent
          title={t('production.title')}
          description={t('production.description')}
          footer={
            <>
              <Button onClick={onCancel}>{t('production.cancel')}</Button>
              <Button
                variant="danger"
                disabled={typed !== request.confirmText}
                onClick={() => {
                  onConfirm({ token: request.token, typed });
                  setTyped('');
                }}
              >
                {t('production.run')}
              </Button>
            </>
          }
        >
          <div className="space-y-3 text-xs">
            {request.statements.map((s) => (
              <pre
                key={s.index}
                className="max-h-40 overflow-auto rounded border border-border bg-surface-muted p-2 font-mono"
              >
                {s.text}
              </pre>
            ))}
            <label className="block space-y-1">
              <span>{t('production.type', { text: request.confirmText })}</span>
              <Input
                value={typed}
                onChange={(e) => setTyped(e.target.value)}
                aria-label={t('production.type', { text: request.confirmText })}
                autoFocus
              />
            </label>
          </div>
        </DialogContent>
      )}
    </Dialog>
  );
}
