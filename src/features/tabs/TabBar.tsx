import { X } from 'lucide-react';
import type { KeyboardEvent, MouseEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { connectionColor, useConnectionProfile } from '@/features/connections/queries';
import { cn } from '@/lib/cn';
import { useTabsStore, type TabItem, type TabTitle } from '@/stores/tabs';

function useTabTitle(title: TabTitle): string {
  const { t } = useTranslation();
  return typeof title === 'string' ? title : t(title.key, { ns: title.ns ?? 'common' });
}

function TabButton({ tab, active }: { tab: TabItem; active: boolean }) {
  const { t } = useTranslation();
  const activate = useTabsStore((s) => s.activate);
  const closeTab = useTabsStore((s) => s.closeTab);
  const title = useTabTitle(tab.title);
  const profile = useConnectionProfile(tab.payload?.connectionId ?? '');
  const color = connectionColor(profile);

  // Middle click closes the tab (auxclick; mousedown would also start autoscroll).
  const onAuxClick = (e: MouseEvent) => {
    if (e.button === 1 && tab.closable) {
      e.preventDefault();
      closeTab(tab.id);
    }
  };

  return (
    <div
      className={cn(
        'group relative flex h-8 items-center rounded-t-md border border-b-0',
        active
          ? 'border-border bg-background text-foreground'
          : 'border-transparent text-muted-foreground hover:text-foreground',
      )}
      onAuxClick={onAuxClick}
      onMouseDown={(e) => e.button === 1 && e.preventDefault()}
    >
      {color && (
        <span
          data-testid="tab-color"
          className="absolute inset-x-0 top-0 h-0.5 rounded-t-md"
          style={{ backgroundColor: color }}
        />
      )}
      <button
        role="tab"
        type="button"
        aria-selected={active}
        tabIndex={active ? 0 : -1}
        data-tab-id={tab.id}
        title={title}
        onClick={() => activate(tab.id)}
        className={cn(
          'h-full max-w-56 truncate pl-3 text-[13px]',
          tab.closable ? 'pr-1' : 'pr-3',
          active && 'font-medium',
        )}
      >
        {title}
      </button>
      {tab.closable && (
        <button
          type="button"
          aria-label={`${t('tabs.close')}: ${title}`}
          tabIndex={-1}
          onClick={() => closeTab(tab.id)}
          className="mr-1 rounded p-0.5 text-muted-foreground opacity-60 hover:bg-surface-muted hover:opacity-100 group-hover:opacity-100"
        >
          <X size={12} />
        </button>
      )}
    </div>
  );
}

export function TabBar() {
  const tabs = useTabsStore((s) => s.tabs);
  const activeId = useTabsStore((s) => s.activeId);
  const activate = useTabsStore((s) => s.activate);
  const closeTab = useTabsStore((s) => s.closeTab);

  const focusTab = (list: HTMLElement, id: string | undefined) => {
    if (!id) return;
    const el = Array.from(list.querySelectorAll<HTMLElement>('[data-tab-id]')).find(
      (e) => e.dataset.tabId === id,
    );
    el?.focus();
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const list = e.currentTarget;
    const current = (e.target as HTMLElement).closest<HTMLElement>('[data-tab-id]');
    if (!current) return;
    const id = current.dataset.tabId!;
    const index = tabs.findIndex((t) => t.id === id);
    const go = (target: number) => {
      const next = tabs[(target + tabs.length) % tabs.length];
      if (!next) return;
      e.preventDefault();
      activate(next.id);
      focusTab(list, next.id);
    };
    if (e.key === 'ArrowRight') go(index + 1);
    else if (e.key === 'ArrowLeft') go(index - 1);
    else if (e.key === 'Home') go(0);
    else if (e.key === 'End') go(tabs.length - 1);
    else if (e.key === 'Delete' || ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'w')) {
      if (!tabs[index]?.closable) return;
      e.preventDefault();
      closeTab(id);
      // The store picks the neighbor; focus it once React has rendered.
      setTimeout(() => focusTab(list, useTabsStore.getState().activeId), 0);
    }
  };

  return (
    <div
      role="tablist"
      onKeyDown={onKeyDown}
      className="flex h-9 shrink-0 items-end gap-px overflow-x-auto border-b border-border bg-surface-muted px-1"
    >
      {tabs.map((tab) => (
        <TabButton key={tab.id} tab={tab} active={tab.id === activeId} />
      ))}
    </div>
  );
}
