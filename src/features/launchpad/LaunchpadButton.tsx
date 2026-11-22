import { Rocket } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { formatKeys } from '@/features/editor/shortcuts';
import { cn } from '@/lib/cn';
import { useAttentionCount } from './hooks';
import { useLaunchpad } from './store';

/** First item of the tab bar: opens or closes the Launchpad; the badge counts what needs you. */
export function LaunchpadButton() {
  const { t } = useTranslation('launchpad');
  const open = useLaunchpad((s) => s.open);
  const toggle = useLaunchpad((s) => s.toggle);
  const count = useAttentionCount();
  const label = `${t('title')} (${formatKeys('Mod-Shift-k')})`;
  return (
    <button
      type="button"
      aria-pressed={open}
      aria-label={count ? `${label}: ${t('badge', { count })}` : label}
      title={label}
      onClick={toggle}
      data-testid="launchpad-button"
      className={cn(
        'relative flex h-9 shrink-0 items-center gap-1.5 rounded-t-md border border-b-0 px-2.5 text-[13px]',
        open
          ? 'border-border bg-background font-medium text-foreground'
          : 'border-transparent text-muted-foreground hover:text-foreground',
      )}
    >
      {open && <span className="absolute inset-x-0 top-0 h-0.5 rounded-t-md bg-accent" />}
      <Rocket size={14} />
      <span className="hidden xl:inline">{t('title')}</span>
      {count > 0 && (
        <span className="min-w-4 rounded-full bg-danger px-1 text-center text-[10px] leading-4 font-semibold text-accent-foreground">
          {count > 99 ? '99+' : count}
        </span>
      )}
    </button>
  );
}
