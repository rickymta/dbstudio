import { useId, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';

interface PasswordDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  connectionName: string;
  onSubmit: (password: string) => void;
}

/** Asks for the password of a profile that does not store one (CONN-08). */
export function PasswordDialog({
  open,
  onOpenChange,
  connectionName,
  onSubmit,
}: PasswordDialogProps) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      {open && (
        <PasswordDialogBody
          connectionName={connectionName}
          onSubmit={onSubmit}
          onClose={() => onOpenChange(false)}
        />
      )}
    </Dialog>
  );
}

function PasswordDialogBody({
  connectionName,
  onSubmit,
  onClose,
}: {
  connectionName: string;
  onSubmit: (password: string) => void;
  onClose: () => void;
}) {
  const { t } = useTranslation('connections');
  const id = useId();
  const [password, setPassword] = useState('');
  return (
    <DialogContent
      title={t('password.title')}
      description={t('password.description', { name: connectionName })}
      className="w-[min(380px,92vw)]"
      footer={
        <>
          <Button onClick={onClose}>{t('dialog.cancel')}</Button>
          <Button type="submit" form={id} variant="primary">
            {t('password.connect')}
          </Button>
        </>
      }
    >
      <form
        id={id}
        className="flex flex-col gap-1"
        onSubmit={(e) => {
          e.preventDefault();
          onSubmit(password);
          onClose();
        }}
      >
        <Label htmlFor={`${id}-pw`}>{t('field.password')}</Label>
        <Input
          id={`${id}-pw`}
          type="password"
          autoFocus
          autoComplete="off"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
        />
      </form>
    </DialogContent>
  );
}
