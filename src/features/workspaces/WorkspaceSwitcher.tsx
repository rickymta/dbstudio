import { Check, ChevronDown, LayoutGrid, Plus } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/DropdownMenu';
import { cn } from '@/lib/cn';
import { useSessionsStore } from '@/stores/sessions';
import { useTabsStore, WELCOME_TAB } from '@/stores/tabs';
import { useLaunchpad } from '@/features/launchpad/store';
import { openWorkspaceDialog } from './dialogs';
import { ROOT } from './model';
import { useActiveWorkspace, useWorkspaces } from './store';

/**
 * Workspace menu of the tab bar: actions on the active workspace, every workspace (picking one
 * swaps the sidebar's connections and the tab set), "No workspace" and "New workspace…".
 * Re-picking the active workspace shows its overview.
 */
export function WorkspaceSwitcher() {
  const { t } = useTranslation('workspaces');
  const items = useWorkspaces((s) => s.items);
  const activate = useWorkspaces((s) => s.activate);
  const addToActive = useWorkspaces((s) => s.addToActive);
  const folder = useWorkspaces((s) => s.folder);
  const ws = useActiveWorkspace();
  const activeTab = useTabsStore((s) => s.tabs.find((x) => x.id === s.activeId));
  const sessions = useSessionsStore((s) => s.sessionsByConnection);
  const tabConnection = activeTab?.payload?.connectionId ?? null;
  const openNotIn = ws ? Object.keys(sessions).filter((id) => !ws.connections.includes(id)) : [];

  const showOverview = () => {
    useLaunchpad.getState().setOpen(false);
    useTabsStore.getState().activate(WELCOME_TAB.id);
  };

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        data-testid="workspace-switcher"
        title={ws ? t('switcher.active', { name: ws.name }) : t('switcher.none')}
        className={cn(
          'flex h-8 max-w-48 shrink-0 items-center gap-1.5 rounded px-2 text-[13px] hover:bg-surface',
          ws ? 'text-foreground' : 'text-muted-foreground',
        )}
      >
        {ws ? (
          <span className="size-2.5 shrink-0 rounded-full" style={{ backgroundColor: ws.color }} />
        ) : (
          <LayoutGrid size={14} />
        )}
        <span className="hidden min-w-0 truncate lg:inline">{ws ? ws.name : t('workspaces')}</span>
        <ChevronDown size={12} className="shrink-0 text-muted-foreground" />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="min-w-64">
        {ws && (
          <>
            <DropdownMenuLabel>{ws.name}</DropdownMenuLabel>
            <DropdownMenuItem onSelect={showOverview}>{t('switcher.overview')}</DropdownMenuItem>
            <DropdownMenuItem
              onSelect={() => openWorkspaceDialog({ type: 'add', workspaceId: ws.id, folder })}
            >
              {t('switcher.addSaved', { name: ws.name })}
            </DropdownMenuItem>
            {openNotIn.length > 0 && (
              <DropdownMenuItem onSelect={() => addToActive(openNotIn)}>
                {t('switcher.addOpen', { count: openNotIn.length, name: ws.name })}
              </DropdownMenuItem>
            )}
            {tabConnection && !ws.connections.includes(tabConnection) && (
              <DropdownMenuItem onSelect={() => addToActive([tabConnection])}>
                {t('switcher.addTab', { name: ws.name })}
              </DropdownMenuItem>
            )}
            <DropdownMenuSeparator />
          </>
        )}
        <DropdownMenuLabel>{t('workspaces')}</DropdownMenuLabel>
        {items.map((w) => (
          <DropdownMenuItem
            key={w.id}
            onSelect={() => (w.id === ws?.id ? showOverview() : activate(w.id))}
          >
            <span className="size-2.5 shrink-0 rounded-full" style={{ backgroundColor: w.color }} />
            <span className="min-w-0 truncate">{w.name}</span>
            <span className="ml-auto pl-3 text-xs text-muted-foreground">
              {w.connections.length}
            </span>
            {w.id === ws?.id && <Check size={13} className="text-accent" />}
          </DropdownMenuItem>
        ))}
        <DropdownMenuItem onSelect={() => activate(ROOT)}>
          <span className="size-2.5 shrink-0" />
          <span>{t('switcher.noWorkspace')}</span>
          {!ws && <Check size={13} className="ml-auto text-accent" />}
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem onSelect={() => openWorkspaceDialog({ type: 'new' })}>
          <Plus size={13} /> {t('switcher.new')}
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
