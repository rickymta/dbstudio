import { useId, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';

interface GroupDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  mode: 'create' | 'rename';
  initialName?: string;
  onSubmit: (name: string) => void;
}

/** Small dialog asking for a group name (create, create sub-group, rename). */
export function GroupDialog({ open, onOpenChange, mode, initialName, onSubmit }: GroupDialogProps) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      {open && (
        <GroupDialogBody
          mode={mode}
          initialName={initialName ?? ''}
          onSubmit={onSubmit}
          onClose={() => onOpenChange(false)}
        />
      )}
    </Dialog>
  );
}

function GroupDialogBody({
  mode,
  initialName,
  onSubmit,
  onClose,
}: {
  mode: 'create' | 'rename';
  initialName: string;
  onSubmit: (name: string) => void;
  onClose: () => void;
}) {
  const { t } = useTranslation('connections');
  const id = useId();
  const [name, setName] = useState(initialName);
  const [touched, setTouched] = useState(false);
  const invalid = touched && !name.trim();
  return (
    <DialogContent
      title={mode === 'create' ? t('group.newTitle') : t('group.renameTitle')}
      className="w-[min(380px,92vw)]"
      footer={
        <>
          <Button onClick={onClose}>{t('dialog.cancel')}</Button>
          <Button type="submit" form={id} variant="primary">
            {mode === 'create' ? t('group.create') : t('dialog.save')}
          </Button>
        </>
      }
    >
      <form
        id={id}
        noValidate
        className="flex flex-col gap-1"
        onSubmit={(e) => {
          e.preventDefault();
          setTouched(true);
          if (!name.trim()) return;
          onSubmit(name.trim());
          onClose();
        }}
      >
        <Label htmlFor={`${id}-name`}>{t('group.name')}</Label>
        <Input
          id={`${id}-name`}
          autoFocus
          value={name}
          aria-invalid={invalid}
          onChange={(e) => setName(e.target.value)}
        />
        {invalid && (
          <p role="alert" className="text-xs text-danger">
            {t('validation.nameRequired')}
          </p>
        )}
      </form>
    </DialogContent>
  );
}
