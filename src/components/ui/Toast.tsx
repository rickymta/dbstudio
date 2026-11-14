import { AlertCircle, CheckCircle2, Info, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';
import { useToastStore, type ToastTone } from './toastStore';

const tones: Record<ToastTone, { icon: typeof Info; className: string }> = {
  info: { icon: Info, className: 'text-accent' },
  success: { icon: CheckCircle2, className: 'text-success' },
  error: { icon: AlertCircle, className: 'text-danger' },
};

/** Mount once near the app root; renders toasts pushed through `toast.*`. */
export function Toaster() {
  const { t } = useTranslation();
  const toasts = useToastStore((s) => s.toasts);
  const dismiss = useToastStore((s) => s.dismiss);
  return (
    <div
      aria-live="polite"
      className="pointer-events-none fixed right-3 bottom-9 z-[60] flex w-80 flex-col gap-2"
    >
      {toasts.map((item) => {
        const { icon: Icon, className } = tones[item.tone];
        return (
          <div
            key={item.id}
            role={item.tone === 'error' ? 'alert' : 'status'}
            className="pointer-events-auto flex items-start gap-2 rounded-md border border-border bg-surface p-2.5 text-[13px] shadow-lg"
          >
            <Icon size={16} className={cn('mt-px shrink-0', className)} />
            <p className="min-w-0 flex-1 select-text break-words">{item.message}</p>
            {item.action && (
              <button
                type="button"
                onClick={() => {
                  dismiss(item.id);
                  item.action?.onClick();
                }}
                className="shrink-0 rounded border border-border px-1.5 py-0.5 text-xs font-medium hover:bg-surface-muted"
              >
                {item.action.label}
              </button>
            )}
            <button
              type="button"
              aria-label={t('toast.dismiss')}
              onClick={() => dismiss(item.id)}
              className="shrink-0 rounded p-0.5 text-muted-foreground hover:bg-surface-muted"
            >
              <X size={12} />
            </button>
          </div>
        );
      })}
    </div>
  );
}
