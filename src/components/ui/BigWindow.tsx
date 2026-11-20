import { X } from 'lucide-react';
import { Dialog as DialogPrimitive } from 'radix-ui';
import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';

interface BigWindowProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  /** Controls at the right of the title bar, before the close button. */
  headerExtra?: ReactNode;
  /** CSS sizes, e.g. `min(720px, calc(100vw - 32px))`. */
  width: string;
  height: string;
  children: ReactNode;
}

/** Large modal window with a title bar and a body the caller lays out (logs, cheat sheets). */
export function BigWindow({
  open,
  onOpenChange,
  title,
  headerExtra,
  width,
  height,
  children,
}: BigWindowProps) {
  const { t } = useTranslation();
  return (
    <DialogPrimitive.Root open={open} onOpenChange={onOpenChange}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay className="fixed inset-0 z-40 bg-black/40" />
        <DialogPrimitive.Content
          aria-describedby={undefined}
          className="fixed top-1/2 left-1/2 z-50 flex -translate-x-1/2 -translate-y-1/2 flex-col overflow-hidden rounded-lg border border-border bg-background text-foreground shadow-xl"
          style={{ width, height }}
        >
          <header className="flex h-11 shrink-0 items-center gap-3 border-b border-border bg-surface px-4">
            <DialogPrimitive.Title className="text-sm font-semibold">{title}</DialogPrimitive.Title>
            <div className="ml-auto flex items-center gap-2">
              {headerExtra}
              <DialogPrimitive.Close
                aria-label={t('actions.close')}
                className="flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-surface-muted hover:text-foreground"
              >
                <X size={14} />
              </DialogPrimitive.Close>
            </div>
          </header>
          {children}
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}
