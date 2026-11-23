import { X, Pin, PanelLeft, ChevronDown, Plus } from 'lucide-react';
import { useEffect, useRef, type KeyboardEvent, type MouseEvent } from 'react';
import { Button } from '@/components/ui/Button';
import { IconButton } from '@/components/ui/IconButton';
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
} from '@/components/ui/DropdownMenu';
import { useWorkspaceStore } from '@/features/workspace/layout';
import { openEditorForActiveTab } from '@/features/editor/open';
import { useSessionsStore } from '@/stores/sessions';
import { useConnectionsSnapshot } from '@/features/connections/queries';
import { formatKeys } from '@/features/editor/shortcuts';
import { useTranslation } from 'react-i18next';
import { connectionColor, useConnectionProfile } from '@/features/connections/queries';
import { cn } from '@/lib/cn';
import { isDirty, useEditorStore } from '@/features/editor/editorStore';
import { useTabsStore, WELCOME_TAB, type TabItem, type TabTitle } from '@/stores/tabs';
import { useActiveWorkspace } from '@/features/workspaces/store';
import { WorkspaceSwitcher } from '@/features/workspaces/WorkspaceSwitcher';
import { LaunchpadButton } from '@/features/launchpad/LaunchpadButton';
import { useLaunchpad } from '@/features/launchpad/store';

function useTabTitle(title: TabTitle): string {
  const { t } = useTranslation();
  return typeof title === 'string' ? title : t(title.key, { ns: title.ns ?? 'common' });
}

function TabButton({
  tab,
  active,
  duplicate,
}: {
  tab: TabItem;
  active: boolean;
  duplicate: boolean;
}) {
  const { t } = useTranslation();
  const activate = useTabsStore((s) => s.activate);
  const closeTab = useTabsStore((s) => s.requestClose);
  const workspace = useActiveWorkspace();
  const baseTitle = useTabTitle(tab.title);
  // The first tab shows the active workspace's overview: it carries the workspace's name.
  const title = tab.id === WELCOME_TAB.id && workspace ? workspace.name : baseTitle;
  const profile = useConnectionProfile(tab.payload?.connectionId ?? '');
  const color = connectionColor(profile);
  const togglePin = useTabsStore((s) => s.togglePin);
  // An editor tab whose file has unsaved changes shows a dot after its title.
  const unsaved = useEditorStore((s) => {
    const data = tab.kind === 'editor' ? s.tabs[tab.payload.tabStateId] : undefined;
    return !!data && isDirty(data);
  });
  const path =
    tab.payload && 'path' in tab.payload
      ? tab.payload.path.segments
          .filter((s) => s.kind !== 'folder')
          .map((s) => s.name)
          .join('.')
      : '';
  const outside =
    !!workspace &&
    !!tab.payload?.connectionId &&
    !workspace.connections.includes(tab.payload.connectionId);
  const detail = [
    title,
    profile?.name,
    path,
    outside ? t('workspaces:notInWorkspace', { name: workspace.name }) : '',
  ]
    .filter(Boolean)
    .join(' · ');

  // Middle click closes the tab (auxclick; mousedown would also start autoscroll).
  const onAuxClick = (e: MouseEvent) => {
    if (e.button === 1 && tab.closable) {
      e.preventDefault();
      void closeTab(tab.id);
    }
  };

  return (
    <div
      className={cn(
        'group relative flex h-9 shrink-0 items-center rounded-t-md border border-b-0',
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
        title={detail}
        onClick={() => {
          // Picking a tab shows it, also the one already active under the Launchpad.
          useLaunchpad.getState().setOpen(false);
          activate(tab.id);
        }}
        className={cn(
          'h-full max-w-56 truncate pl-3 text-[13px]',
          tab.closable ? 'pr-1' : 'pr-3',
          active && 'font-medium',
          outside && 'italic',
        )}
      >
        {title}
        {unsaved && (
          <span data-testid="tab-unsaved" aria-label={t('tabs.unsaved')} title={t('tabs.unsaved')}>
            {' •'}
          </span>
        )}
        {duplicate && (
          <span className="ml-2 text-xs text-muted-foreground">
            {profile?.name}
            {path ? ` · ${path}` : ''}
          </span>
        )}
      </button>
      {tab.closable && tab.kind !== 'welcome' && (
        <button
          type="button"
          aria-label={`${t(tab.pinned ? 'workspace.unpinTab' : 'workspace.pinTab')}: ${title}`}
          aria-pressed={!!tab.pinned}
          onClick={() => togglePin(tab.id)}
          className="flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-surface-muted"
        >
          <Pin size={12} className={tab.pinned ? 'fill-current text-accent' : 'opacity-40'} />
        </button>
      )}
      {tab.closable && !tab.pinned && (
        <button
          type="button"
          aria-label={`${t('tabs.close')}: ${title}`}
          tabIndex={-1}
          onClick={() => void closeTab(tab.id)}
          className="mr-1 flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground opacity-60 hover:bg-surface-muted hover:opacity-100 group-hover:opacity-100"
        >
          <X size={12} />
        </button>
      )}
    </div>
  );
}

export function TabBar() {
  const { t } = useTranslation();
  const listRef = useRef<HTMLDivElement>(null);
  const hidden = useWorkspaceStore((s) => s.sidebarHidden);
  const toggleSidebar = useWorkspaceStore((s) => s.toggleSidebar);
  const sessions = useSessionsStore((s) => s.sessionsByConnection);
  const { data: connections } = useConnectionsSnapshot();
  const tabs = useTabsStore((s) => s.tabs);
  const activeId = useTabsStore((s) => s.activeId);
  const activate = useTabsStore((s) => s.activate);
  const closeTab = useTabsStore((s) => s.requestClose);
  const launchpadOpen = useLaunchpad((s) => s.open);
  const workspace = useActiveWorkspace();

  const ordered = [...tabs.filter((t) => t.pinned), ...tabs.filter((t) => !t.pinned)];
  const label = (tab: TabItem) =>
    tab.kind === 'welcome' && workspace
      ? workspace.name
      : typeof tab.title === 'string'
        ? tab.title
        : t(tab.title.key, { ns: tab.title.ns ?? 'common' });
  useEffect(() => {
    const active = Array.from(
      listRef.current?.querySelectorAll<HTMLElement>('[data-tab-id]') ?? [],
    ).find((el) => el.dataset.tabId === activeId);
    active?.scrollIntoView?.({ block: 'nearest', inline: 'nearest' });
  }, [activeId]);
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
    const index = ordered.findIndex((t) => t.id === id);
    const go = (target: number) => {
      const next = ordered[(target + ordered.length) % ordered.length];
      if (!next) return;
      e.preventDefault();
      activate(next.id);
      focusTab(list, next.id);
    };
    if (e.key === 'ArrowRight') go(index + 1);
    else if (e.key === 'ArrowLeft') go(index - 1);
    else if (e.key === 'Home') go(0);
    else if (e.key === 'End') go(ordered.length - 1);
    else if (e.key === 'Delete' || ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'w')) {
      if (!ordered[index]?.closable) return;
      e.preventDefault();
      void closeTab(id);
      // The store picks the neighbor; focus it once React has rendered.
      setTimeout(() => focusTab(list, useTabsStore.getState().activeId), 0);
    }
  };

  return (
    <div className="flex min-h-10 shrink-0 items-center border-b border-border bg-surface-muted px-1">
      <IconButton
        label={`${t('workspace.toggleSidebar')} (${formatKeys('Mod-b')})`}
        icon={<PanelLeft size={16} />}
        aria-pressed={!hidden}
        onClick={toggleSidebar}
      />
      <LaunchpadButton />
      <WorkspaceSwitcher />
      <div
        ref={listRef}
        role="tablist"
        onKeyDown={onKeyDown}
        className="flex min-w-0 flex-1 items-end gap-px overflow-x-auto"
      >
        {ordered.map((tab) => (
          <TabButton
            key={tab.id}
            tab={tab}
            active={!launchpadOpen && tab.id === activeId}
            duplicate={tabs.some((other) => other.id !== tab.id && label(other) === label(tab))}
          />
        ))}
      </div>
      <IconButton
        label={`${t('workspace.newQuery')} (${formatKeys('Mod-t')})`}
        icon={<Plus size={16} />}
        disabled={!Object.values(sessions).some((s) => s.descriptor.capabilities.leases)}
        onClick={() => openEditorForActiveTab()}
      />
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button size="icon" variant="ghost" aria-label={t('workspace.openTabs')}>
            <ChevronDown size={16} />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="max-h-80 max-w-[min(480px,90vw)] overflow-auto">
          {!tabs.some((tab) => tab.kind === 'welcome') && (
            <DropdownMenuItem
              onSelect={() => {
                useLaunchpad.getState().setOpen(false);
                useTabsStore.getState().openTab(WELCOME_TAB);
              }}
            >
              {workspace ? t('workspaces:switcher.overview') : t('welcome.reopen')}
            </DropdownMenuItem>
          )}
          {ordered.map((tab) => (
            <DropdownMenuItem
              key={tab.id}
              onSelect={() => {
                useLaunchpad.getState().setOpen(false);
                activate(tab.id);
              }}
            >
              <span className="truncate">
                {tab.pinned ? '• ' : ''}
                {label(tab)}
              </span>
              <span className="ml-auto truncate text-xs text-muted-foreground">
                {connections?.profiles.find((p) => p.id === tab.payload?.connectionId)?.name}
              </span>
            </DropdownMenuItem>
          ))}
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}
