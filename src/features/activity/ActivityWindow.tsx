import { AppWindow, Database, Trash2 } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { BigWindow } from '@/components/ui/BigWindow';
import { IconButton } from '@/components/ui/IconButton';
import { useConnectionProfile } from '@/features/connections/queries';
import { cn } from '@/lib/cn';
import { useActivity, type Activity } from '@/stores/activity';
import { useTabsStore } from '@/stores/tabs';

type Scope = 'app' | 'connection';

/**
 * Activity Logs: what the application did and how it ended, for the application as a whole and
 * for the connection of the active tab.
 */
export function ActivityWindow({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const { t } = useTranslation();
  const connectionId =
    useTabsStore((s) => s.tabs.find((x) => x.id === s.activeId)?.payload?.connectionId) ?? null;
  const profile = useConnectionProfile(connectionId ?? '');
  const items = useActivity((s) => s.items);
  const clear = useActivity((s) => s.clear);
  const [chosen, setChosen] = useState<Scope | null>(null);
  // Until the user picks a tab, show the connection of the active tab when there is one.
  const scope: Scope = connectionId ? (chosen ?? 'connection') : 'app';
  const box = useRef<HTMLDivElement>(null);

  const shown = items.filter((i) =>
    scope === 'app' ? i.connectionId === null : i.connectionId === connectionId,
  );

  // Follow new lines while scrolled to the bottom; start at the bottom when the window opens.
  const wasOpen = useRef(false);
  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
    if (nearBottom || !wasOpen.current) el.scrollTop = el.scrollHeight;
    wasOpen.current = open;
  }, [open, scope, shown.length]);

  const tab = (id: Scope, icon: React.ReactNode, label: string) => (
    <button
      type="button"
      role="tab"
      aria-selected={scope === id}
      disabled={id === 'connection' && !connectionId}
      onClick={() => setChosen(id)}
      className={cn(
        'flex h-full items-center gap-2 px-4 text-[13px] disabled:opacity-40',
        scope === id
          ? 'bg-surface-muted font-medium text-foreground'
          : 'text-muted-foreground hover:text-foreground',
      )}
    >
      {icon}
      {label}
    </button>
  );

  return (
    <BigWindow
      open={open}
      onOpenChange={onOpenChange}
      width="min(1100px, calc(100vw - 64px))"
      height="calc(100vh - 96px)"
      title={t('activity.title')}
      headerExtra={
        <IconButton
          label={t('activity.clear')}
          icon={<Trash2 size={14} />}
          disabled={shown.length === 0}
          onClick={() => clear(scope === 'app' ? null : connectionId)}
        />
      }
    >
      <div
        role="tablist"
        className="flex h-9 shrink-0 items-stretch border-b border-border bg-surface"
      >
        {tab('app', <AppWindow size={14} aria-hidden />, t('activity.application'))}
        {tab(
          'connection',
          <Database size={14} aria-hidden />,
          profile ? `${t('activity.connection')} · ${profile.name}` : t('activity.connection'),
        )}
      </div>
      <div
        ref={box}
        data-testid="activity-log"
        className="min-h-0 flex-1 overflow-auto px-4 py-3 font-mono text-xs leading-5 select-text"
      >
        {shown.length === 0 && (
          <p className="py-10 text-center font-sans text-muted-foreground">{t('activity.empty')}</p>
        )}
        {shown.map((item) => (
          <ActivityLine key={item.id} item={item} />
        ))}
      </div>
    </BigWindow>
  );
}

function ActivityLine({ item }: { item: Activity }) {
  const { t, i18n } = useTranslation();
  const time = new Intl.DateTimeFormat(i18n.language, {
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  }).format(new Date(item.at));
  return (
    <div className="flex gap-3" data-status={item.status}>
      <span className="shrink-0 text-muted-foreground">{time}</span>
      <span
        className={cn(
          'min-w-0 flex-1 whitespace-pre-wrap',
          item.status === 'failed' && 'text-danger',
        )}
      >
        {t(`activity.event.${item.event}`, { defaultValue: item.event })}
        {item.status !== 'info' && `: ${t(`activity.status.${item.status}`)}`}
        {item.error && <span className="block opacity-80">{item.error}</span>}
      </span>
      {item.ms !== undefined && (
        <span className="shrink-0 text-accent">{t('activity.ms', { ms: item.ms })}</span>
      )}
    </div>
  );
}
