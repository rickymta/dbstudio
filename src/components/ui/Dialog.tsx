import { X } from 'lucide-react';
import { Dialog as DialogPrimitive } from 'radix-ui';
import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';

export const Dialog = DialogPrimitive.Root;
export const DialogTrigger = DialogPrimitive.Trigger;
export const DialogClose = DialogPrimitive.Close;

interface DialogContentProps {
  title: string;
  description?: string;
  /** Rendered in a right-aligned footer row (typically buttons). */
  footer?: ReactNode;
  children?: ReactNode;
  className?: string;
}

/** Centered modal with title, optional description, scrollable body and footer. */
export function DialogContent({
  title,
  description,
  footer,
  children,
  className,
}: DialogContentProps) {
  const { t } = useTranslation();
  return (
    <DialogPrimitive.Portal>
      <DialogPrimitive.Overlay className="fixed inset-0 z-40 bg-black/40" />
      <DialogPrimitive.Content
        className={cn(
          'fixed top-1/2 left-1/2 z-50 flex max-h-[85vh] w-[min(520px,92vw)] -translate-x-1/2 -translate-y-1/2',
          'flex-col rounded-lg border border-border bg-surface text-foreground shadow-xl',
          className,
        )}
        {...(description ? {} : { 'aria-describedby': undefined })}
      >
        <header className="flex items-start justify-between gap-3 border-b border-border px-4 py-3">
          <div className="min-w-0">
            <DialogPrimitive.Title className="text-sm font-semibold">{title}</DialogPrimitive.Title>
            {description && (
              <DialogPrimitive.Description className="mt-0.5 text-xs text-muted-foreground">
                {description}
              </DialogPrimitive.Description>
            )}
          </div>
          <DialogPrimitive.Close
            aria-label={t('actions.close')}
            className="flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-surface-muted hover:text-foreground"
          >
            <X size={14} />
          </DialogPrimitive.Close>
        </header>
        <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3">{children}</div>
        {footer && (
          <footer className="flex justify-end gap-2 border-t border-border px-4 py-3">
            {footer}
          </footer>
        )}
      </DialogPrimitive.Content>
    </DialogPrimitive.Portal>
  );
}
