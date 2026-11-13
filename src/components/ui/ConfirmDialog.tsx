import { useTranslation } from 'react-i18next';
import { Button } from './Button';
import { Dialog, DialogClose, DialogContent } from './Dialog';

interface ConfirmDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description?: string;
  confirmLabel?: string;
  cancelLabel?: string;
  /** Style the confirm button as a destructive action. */
  destructive?: boolean;
  onConfirm: () => void;
}

export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  confirmLabel,
  cancelLabel,
  destructive,
  onConfirm,
}: ConfirmDialogProps) {
  const { t } = useTranslation();
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title={title}
        description={description}
        className="w-[min(420px,92vw)]"
        footer={
          <>
            <DialogClose asChild>
              <Button>{cancelLabel ?? t('actions.cancel')}</Button>
            </DialogClose>
            <Button
              variant={destructive ? 'danger' : 'primary'}
              onClick={() => {
                onConfirm();
                onOpenChange(false);
              }}
            >
              {confirmLabel ?? t('actions.confirm')}
            </Button>
          </>
        }
      />
    </Dialog>
  );
}
