import { TriangleAlert } from 'lucide-react';
import { useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import type { ConfirmationGrant, ConfirmationRequest } from '@/lib/types';

export interface ConfirmDestructiveDialogProps {
  /** The `confirmation` of a `confirmationRequired` error; `null` closes the dialog. */
  request: ConfirmationRequest | null;
  /** The user typed `request.confirmText`: call the command again with this grant. */
  onConfirm: (grant: ConfirmationGrant) => void;
  onCancel: () => void;
}

/** Asks the user to type the confirmation text before a destructive statement runs. */
export function ConfirmDestructiveDialog({
  request,
  onConfirm,
  onCancel,
}: ConfirmDestructiveDialogProps): ReactNode {
  // Keyed by token: every request starts with an empty input.
  return request ? (
    <ConfirmForm key={request.token} request={request} onConfirm={onConfirm} onCancel={onCancel} />
  ) : null;
}

function ConfirmForm({
  request,
  onConfirm,
  onCancel,
}: {
  request: ConfirmationRequest;
  onConfirm: (grant: ConfirmationGrant) => void;
  onCancel: () => void;
}): ReactNode {
  const { t } = useTranslation('safety');
  const [typed, setTyped] = useState('');
  // The button works only for the exact text: case and spaces count.
  const ok = typed === request.confirmText;
  const submit = () => {
    if (ok) onConfirm({ token: request.token, typed });
  };

  return (
    <Dialog open onOpenChange={(open) => !open && onCancel()}>
      <DialogContent
        title={t('destructive.title')}
        description={t('destructive.description')}
        className="w-[min(560px,94vw)]"
        footer={
          <>
            <Button onClick={onCancel}>{t('destructive.cancel')}</Button>
            <Button variant="danger" disabled={!ok} onClick={submit}>
              {t('destructive.confirm')}
            </Button>
          </>
        }
      >
        <div className="flex flex-col gap-3">
          <ul
            className="flex max-h-48 flex-col gap-2 overflow-auto"
            aria-label={t('destructive.statements')}
          >
            {request.statements.map((s) => (
              <li key={`${s.index}-${s.range.start}`} className="rounded border border-border">
                <div className="flex items-center gap-1.5 border-b border-border bg-warning-soft px-2 py-1 text-xs font-medium text-warning">
                  <TriangleAlert size={12} aria-hidden />
                  {t(`destructive.reason.${s.reason}`)}
                </div>
                <pre className="max-h-24 overflow-auto px-2 py-1 font-mono text-xs whitespace-pre-wrap">
                  {s.text}
                </pre>
              </li>
            ))}
          </ul>
          <div className="flex flex-col gap-1">
            <Label htmlFor="confirm-destructive-text">
              {t('destructive.typePrompt', { text: request.confirmText })}
            </Label>
            <Input
              id="confirm-destructive-text"
              autoFocus
              autoComplete="off"
              spellCheck={false}
              value={typed}
              placeholder={request.confirmText}
              aria-invalid={typed !== '' && !ok}
              onChange={(e) => setTyped(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault();
                  submit();
                }
              }}
            />
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
