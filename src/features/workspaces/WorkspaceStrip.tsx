import { LayoutGrid, Plus } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { IconButton } from '@/components/ui/IconButton';
import { useTabsStore, WELCOME_TAB } from '@/stores/tabs';
import { openWorkspaceDialog } from './dialogs';
import { useActiveWorkspace, useWorkspaces } from './store';

const showOverview = () => useTabsStore.getState().activate(WELCOME_TAB.id);

/** Sidebar band naming the active workspace (its connections are the only ones listed). */
export function WorkspaceStrip() {
  const { t } = useTranslation('workspaces');
  const ws = useActiveWorkspace();
  const folder = useWorkspaces((s) => s.folder);
  if (!ws) return null;
  return (
    <div
      data-testid="workspace-strip"
      className="flex h-8 shrink-0 items-center gap-1.5 border-b border-border pr-1.5 pl-3 text-xs"
      style={{ boxShadow: `inset 3px 0 0 ${ws.color}` }}
    >
      <span className="min-w-0 truncate font-medium" title={t('strip.title', { name: ws.name })}>
        {ws.name}
      </span>
      <span className="shrink-0 text-muted-foreground">
        {t('connectionCount', { count: ws.connections.length })}
      </span>
      <span className="ml-auto flex shrink-0 items-center">
        <IconButton
          label={t('strip.overview')}
          icon={<LayoutGrid size={13} />}
          onClick={showOverview}
        />
        <IconButton
          label={t('actions.addConnections')}
          icon={<Plus size={13} />}
          onClick={() => openWorkspaceDialog({ type: 'add', workspaceId: ws.id, folder })}
        />
      </span>
    </div>
  );
}

/** Sidebar content of a workspace without connections. */
export function WorkspaceEmptyHint() {
  const { t } = useTranslation('workspaces');
  const ws = useActiveWorkspace();
  if (!ws) return null;
  return (
    <div className="space-y-2 px-4 py-6 text-center text-xs text-muted-foreground">
      <p>{t('emptySidebar')}</p>
      <Button
        size="sm"
        onClick={() => openWorkspaceDialog({ type: 'add', workspaceId: ws.id, folder: '' })}
      >
        <Plus size={12} /> {t('actions.addConnections')}
      </Button>
    </div>
  );
}
