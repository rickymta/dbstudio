import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';
import { useTabsStore } from '@/stores/tabs';

export function TabBar() {
  const { t } = useTranslation();
  const tabs = useTabsStore((s) => s.tabs);
  const activeId = useTabsStore((s) => s.activeId);
  const activate = useTabsStore((s) => s.activate);

  return (
    <div
      role="tablist"
      className="flex h-9 shrink-0 items-end gap-px overflow-x-auto border-b border-border bg-surface-muted px-1"
    >
      {tabs.map((tab) => {
        const active = tab.id === activeId;
        return (
          <button
            key={tab.id}
            role="tab"
            type="button"
            aria-selected={active}
            onClick={() => activate(tab.id)}
            className={cn(
              'h-8 rounded-t-md border border-b-0 px-3 text-[13px]',
              active
                ? 'border-border bg-background font-medium text-foreground'
                : 'border-transparent text-muted-foreground hover:text-foreground',
            )}
          >
            {t(tab.titleKey)}
          </button>
        );
      })}
    </div>
  );
}
