import {
  ChevronDown,
  ChevronRight,
  Ellipsis,
  Folder,
  FolderOpen,
  FolderPlus,
  Layers,
  Plug,
  PlugZap,
  Plus,
  SquareTerminal,
  Unplug,
  X,
} from 'lucide-react';
import { useMemo, useState, type DragEvent, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { confirmDialog } from '@/components/ui/confirm';
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuSub,
  ContextMenuSubContent,
  ContextMenuSubTrigger,
  ContextMenuTrigger,
} from '@/components/ui/ContextMenu';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from '@/components/ui/DropdownMenu';
import { IconButton } from '@/components/ui/IconButton';
import { Input } from '@/components/ui/Input';
import { useConnectErrors } from '@/features/connections/connectErrors';
import { kindBadgeFor } from '@/features/connections/kindConfig';
import { connectionColor, useConnectionsSnapshot } from '@/features/connections/queries';
import { openEditor } from '@/features/editor/open';
import { useNavigation } from '@/features/workspace/navigation';
import { cn } from '@/lib/cn';
import i18n from '@/lib/i18n';
import type { ConnectionProfile, Uuid } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { useTabsStore } from '@/stores/tabs';
import {
  connectMany,
  disconnectMany,
  PROBLEM_STATUSES,
  useConnectionStatus,
  type ConnectionStatus,
} from './connectionStatus';
import { openWorkspaceDialog } from './dialogs';
import * as M from './model';
import { useWorkspaces } from './store';
import { ColorSwatches } from './WorkspaceDialogs';

type Chip = 'all' | 'connected' | 'idle' | 'problems' | 'production';
const CHIPS: Chip[] = ['all', 'connected', 'idle', 'problems', 'production'];

function chipMatches(chip: Chip, p: ConnectionProfile, status: ConnectionStatus): boolean {
  switch (chip) {
    case 'all':
      return true;
    case 'connected':
      return status === 'connected' || status === 'reconnecting';
    case 'idle':
      return status === 'idle' || status === 'opening';
    case 'problems':
      return PROBLEM_STATUSES.has(status);
    case 'production':
      return p.environment === 'production';
  }
}

const STATUS_DOT: Record<ConnectionStatus, string> = {
  connected: 'bg-success',
  opening: 'bg-warning animate-pulse',
  reconnecting: 'bg-warning animate-pulse',
  lost: 'bg-danger',
  gaveUp: 'bg-danger',
  failed: 'bg-danger',
  idle: 'bg-border',
};

// ---- drag and drop (HTML5, like the connections sidebar) ----------------------------------------

type Dragged = { type: 'connections'; ids: Uuid[] } | { type: 'folder'; id: string };
/** `dragover` cannot read `dataTransfer` data: the dragged item lives here. */
let dragging: Dragged | null = null;

function acceptsDrop(ws: M.Workspace, folder: string): boolean {
  if (!dragging) return false;
  if (dragging.type === 'folder') return M.canMove(ws.folders, dragging.id, folder);
  return dragging.ids.some((id) => M.folderOf(ws, id) !== folder);
}

/** Props making an element a drop target for `folder` ([`M.ROOT`] = the workspace root). */
function useDropTarget(ws: M.Workspace, folder: string) {
  const edit = useWorkspaces((s) => s.edit);
  const [over, setOver] = useState(false);
  return {
    over,
    props: {
      'data-drop-folder': folder,
      onDragOver: (e: DragEvent) => {
        if (!acceptsDrop(ws, folder)) return;
        e.preventDefault();
        e.stopPropagation();
        e.dataTransfer.dropEffect = 'move';
        setOver(true);
      },
      onDragLeave: () => setOver(false),
      onDrop: (e: DragEvent) => {
        setOver(false);
        const item = dragging;
        dragging = null;
        if (!item || !acceptsDrop(ws, folder)) {
          if (item) dragging = item;
          return;
        }
        e.preventDefault();
        e.stopPropagation();
        edit(ws.id, (w) =>
          item.type === 'folder'
            ? M.moveFolder(w, item.id, folder)
            : M.moveConnections(w, item.ids, folder),
        );
      },
    },
  };
}

const dragProps = (item: Dragged) => ({
  draggable: true,
  onDragStart: (e: DragEvent) => {
    dragging = item;
    e.dataTransfer.effectAllowed = 'move';
    e.dataTransfer.setData('text/plain', item.type === 'folder' ? item.id : item.ids.join(','));
  },
  onDragEnd: () => {
    dragging = null;
  },
});

// ---- menus (one description, rendered as a dropdown or a context menu) -----------------------

type MenuItemSpec =
  | 'separator'
  | {
      label: string;
      onSelect?: () => void;
      disabled?: boolean;
      danger?: boolean;
      sub?: MenuItemSpec[];
    };

function DropdownItems({ items }: { items: MenuItemSpec[] }) {
  return items.map((item, i) =>
    item === 'separator' ? (
      <DropdownMenuSeparator key={i} />
    ) : item.sub ? (
      <DropdownMenuSub key={i}>
        <DropdownMenuSubTrigger disabled={item.disabled || item.sub.length === 0}>
          {item.label}
        </DropdownMenuSubTrigger>
        <DropdownMenuSubContent>
          <DropdownItems items={item.sub} />
        </DropdownMenuSubContent>
      </DropdownMenuSub>
    ) : (
      <DropdownMenuItem
        key={i}
        disabled={item.disabled}
        onSelect={item.onSelect}
        className={item.danger ? 'text-danger' : undefined}
      >
        {item.label}
      </DropdownMenuItem>
    ),
  );
}

function ContextItems({ items }: { items: MenuItemSpec[] }) {
  return items.map((item, i) =>
    item === 'separator' ? (
      <ContextMenuSeparator key={i} />
    ) : item.sub ? (
      <ContextMenuSub key={i}>
        <ContextMenuSubTrigger disabled={item.disabled || item.sub.length === 0}>
          {item.label}
        </ContextMenuSubTrigger>
        <ContextMenuSubContent>
          <ContextItems items={item.sub} />
        </ContextMenuSubContent>
      </ContextMenuSub>
    ) : (
      <ContextMenuItem
        key={i}
        disabled={item.disabled}
        onSelect={item.onSelect}
        className={item.danger ? 'text-danger' : undefined}
      >
        {item.label}
      </ContextMenuItem>
    ),
  );
}

function MoreMenu({ items, label }: { items: MenuItemSpec[]; label: string }) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button size="icon" variant="ghost" aria-label={label} title={label}>
          <Ellipsis size={14} />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownItems items={items} />
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/** "Move to" targets: the root, then every folder indented by depth (`exclude` disabled). */
function moveTargets(
  ws: M.Workspace,
  allowed: (folder: string) => boolean,
  move: (folder: string) => void,
  rootLabel: string,
): MenuItemSpec[] {
  const out: MenuItemSpec[] = [
    { label: rootLabel, disabled: !allowed(M.ROOT), onSelect: () => move(M.ROOT) },
  ];
  const walk = (parent: string, depth: number) => {
    for (const f of M.childrenOf(ws.folders, parent)) {
      out.push({
        label: `${' '.repeat(depth)}${f.name}`,
        disabled: !allowed(f.id),
        onSelect: () => move(f.id),
      });
      walk(f.id, depth + 1);
    }
  };
  walk(M.ROOT, 1);
  return out;
}

function useFolderMenu(ws: M.Workspace) {
  const { t } = useTranslation('workspaces');
  const edit = useWorkspaces((s) => s.edit);
  const setFolder = useWorkspaces((s) => s.setFolder);
  const shown = useWorkspaces((s) => s.folder);
  return (folder: M.WorkspaceFolder): MenuItemSpec[] => [
    {
      label: t('folder.newSubfolder'),
      disabled: !M.canNest(ws.folders, folder.id),
      onSelect: () => newFolderDialog(ws, folder.id, edit),
    },
    {
      label: t('folder.rename'),
      onSelect: () =>
        openWorkspaceDialog({
          type: 'name',
          title: t('folder.renameTitle'),
          label: t('name'),
          initial: folder.name,
          submitLabel: t('common:actions.save'),
          onSubmit: (name) => edit(ws.id, (w) => M.patchFolder(w, folder.id, { name })),
        }),
    },
    {
      label: t('folder.moveTo'),
      sub: moveTargets(
        ws,
        (target) => target !== folder.parent && M.canMove(ws.folders, folder.id, target),
        (target) => edit(ws.id, (w) => M.moveFolder(w, folder.id, target)),
        t('allConnections'),
      ),
    },
    'separator',
    {
      label: t('folder.delete'),
      danger: true,
      onSelect: () => {
        const parent = M.pathTo(ws.folders, folder.parent).at(-1)?.name ?? t('allConnections');
        void confirmDialog({
          title: t('folder.deleteTitle', { name: folder.name }),
          description: t('folder.deleteBody', { parent }),
          confirmLabel: t('common:actions.delete'),
          destructive: true,
        }).then((ok) => {
          if (!ok) return;
          edit(ws.id, (w) => M.deleteFolder(w, folder.id));
          if (M.subtree(ws.folders, folder.id).has(shown)) setFolder(folder.parent);
        });
      },
    },
  ];
}

function newFolderDialog(
  ws: M.Workspace,
  parent: string,
  edit: (id: string, fn: (w: M.Workspace) => M.Workspace) => void,
  then?: (folderId: string) => void,
) {
  openWorkspaceDialog({
    type: 'name',
    title: i18n.t(parent ? 'workspaces:folder.newSubfolder' : 'workspaces:folder.new'),
    label: i18n.t('workspaces:name'),
    initial: '',
    submitLabel: i18n.t('workspaces:folder.create'),
    onSubmit: (name) => {
      const id = M.newId();
      edit(ws.id, (w) => {
        const opened = parent ? M.patchFolder(w, parent, { collapsed: false }) : w;
        return M.addFolder(opened, parent, name, id);
      });
      then?.(id);
    },
  });
}

// ---- folder tree (left column) ---------------------------------------------------------------

function TreeNode({
  ws,
  folder,
  depth,
  menu,
}: {
  ws: M.Workspace;
  folder: M.WorkspaceFolder;
  depth: number;
  menu: (f: M.WorkspaceFolder) => MenuItemSpec[];
}) {
  const { t } = useTranslation('workspaces');
  const shown = useWorkspaces((s) => s.folder);
  const setFolder = useWorkspaces((s) => s.setFolder);
  const edit = useWorkspaces((s) => s.edit);
  const drop = useDropTarget(ws, folder.id);
  const kids = M.childrenOf(ws.folders, folder.id);
  const count = M.connectionsIn(ws, folder.id, true).length;
  const Icon = folder.collapsed || kids.length === 0 ? Folder : FolderOpen;
  return (
    <li>
      <ContextMenu>
        <ContextMenuTrigger asChild>
          <div
            {...drop.props}
            {...dragProps({ type: 'folder', id: folder.id })}
            role="treeitem"
            aria-selected={shown === folder.id}
            aria-expanded={kids.length ? !folder.collapsed : undefined}
            onClick={() => setFolder(folder.id)}
            style={{ paddingLeft: 6 + depth * 12 }}
            className={cn(
              'group flex h-7 cursor-default items-center gap-1 rounded pr-1 text-[13px] hover:bg-surface-muted',
              shown === folder.id && 'bg-surface-muted font-medium',
              drop.over && 'ring-1 ring-accent',
            )}
          >
            <button
              type="button"
              aria-label={folder.collapsed ? t('folder.expand') : t('folder.collapse')}
              tabIndex={-1}
              onClick={(e) => {
                e.stopPropagation();
                edit(ws.id, (w) => M.patchFolder(w, folder.id, { collapsed: !folder.collapsed }));
              }}
              className={cn(
                'flex size-4 items-center justify-center text-muted-foreground',
                !kids.length && 'invisible',
              )}
            >
              {folder.collapsed ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
            </button>
            <Icon size={14} className="shrink-0" style={{ color: ws.color }} />
            <span className="min-w-0 flex-1 truncate">{folder.name}</span>
            <span className="text-xs text-muted-foreground group-hover:hidden">{count}</span>
            <span className="hidden group-hover:block" onClick={(e) => e.stopPropagation()}>
              <MoreMenu items={menu(folder)} label={t('folder.menu')} />
            </span>
          </div>
        </ContextMenuTrigger>
        <ContextMenuContent>
          <ContextItems items={menu(folder)} />
        </ContextMenuContent>
      </ContextMenu>
      {!folder.collapsed && kids.length > 0 && (
        <ul role="group">
          {kids.map((k) => (
            <TreeNode key={k.id} ws={ws} folder={k} depth={depth + 1} menu={menu} />
          ))}
        </ul>
      )}
    </li>
  );
}

function FolderTree({
  ws,
  menu,
}: {
  ws: M.Workspace;
  menu: (f: M.WorkspaceFolder) => MenuItemSpec[];
}) {
  const { t } = useTranslation('workspaces');
  const shown = useWorkspaces((s) => s.folder);
  const setFolder = useWorkspaces((s) => s.setFolder);
  const edit = useWorkspaces((s) => s.edit);
  const root = useDropTarget(ws, M.ROOT);
  return (
    <nav aria-label={t('folders')} className="self-start @3xl:sticky @3xl:top-0">
      <div className="mb-1 flex items-center justify-between px-1.5">
        <h3 className="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
          {t('folders')}
        </h3>
        <IconButton
          label={t('folder.new')}
          icon={<FolderPlus size={13} />}
          onClick={() => newFolderDialog(ws, M.ROOT, edit)}
        />
      </div>
      <ul role="tree" aria-label={t('folders')}>
        <li>
          <div
            {...root.props}
            role="treeitem"
            aria-selected={shown === M.ROOT}
            onClick={() => setFolder(M.ROOT)}
            className={cn(
              'flex h-7 cursor-default items-center gap-1.5 rounded px-1.5 text-[13px] hover:bg-surface-muted',
              shown === M.ROOT && 'bg-surface-muted font-medium',
              root.over && 'ring-1 ring-accent',
            )}
          >
            <Layers size={14} style={{ color: ws.color }} />
            <span className="min-w-0 flex-1 truncate">{t('allConnections')}</span>
            <span className="text-xs text-muted-foreground">{ws.connections.length}</span>
          </div>
        </li>
        {M.childrenOf(ws.folders, M.ROOT).map((f) => (
          <TreeNode key={f.id} ws={ws} folder={f} depth={0} menu={menu} />
        ))}
      </ul>
      {ws.folders.length === 0 && (
        <button
          type="button"
          onClick={() => newFolderDialog(ws, M.ROOT, edit)}
          className="mt-2 w-full rounded-md border border-dashed border-border p-2 text-left text-xs text-muted-foreground hover:bg-surface-muted"
        >
          {t('folder.hint', { max: M.MAX_DEPTH })}
        </button>
      )}
    </nav>
  );
}

// ---- table rows -----------------------------------------------------------------------------

interface RowContext {
  ws: M.Workspace;
  profiles: Map<Uuid, ConnectionProfile>;
  status: (id: Uuid) => ConnectionStatus;
  visible: (p: ConnectionProfile) => boolean;
  filtering: boolean;
  menu: (f: M.WorkspaceFolder) => MenuItemSpec[];
  tabCount: (id: Uuid) => number;
}

// Columns: connection | type | environment | status | tabs | actions. A narrow pane drops type and
// tabs (below @4xl), then environment (below @2xl); a `hidden` cell takes no grid slot.
const GRID =
  'grid items-center gap-3 grid-cols-[minmax(0,1fr)_minmax(0,9rem)_auto] ' +
  '@2xl:grid-cols-[minmax(0,1fr)_minmax(0,7rem)_minmax(0,9rem)_auto] ' +
  '@4xl:grid-cols-[minmax(0,2.2fr)_90px_minmax(0,1fr)_minmax(0,1.1fr)_60px_auto]';
const KIND_CELL = 'hidden @4xl:block';
const ENV_CELL = 'hidden @2xl:block';
const TABS_CELL = 'hidden @4xl:block';

function FolderRow({
  folder,
  depth,
  ctx,
}: {
  folder: M.WorkspaceFolder;
  depth: number;
  ctx: RowContext;
}) {
  const { t } = useTranslation('workspaces');
  const { ws } = ctx;
  const edit = useWorkspaces((s) => s.edit);
  const setFolder = useWorkspaces((s) => s.setFolder);
  const drop = useDropTarget(ws, folder.id);
  const all = M.connectionsIn(ws, folder.id, true)
    .map((id) => ctx.profiles.get(id))
    .filter((p): p is ConnectionProfile => !!p);
  const hits = all.filter(ctx.visible);
  if (ctx.filtering && hits.length === 0) return null;
  const open = ctx.filtering || !folder.collapsed;
  const connected = all.filter((p) => ctx.status(p.id) === 'connected').length;
  const problems = all.filter((p) => PROBLEM_STATUSES.has(ctx.status(p.id))).length;
  const items: MenuItemSpec[] = [
    { label: t('folder.showOnly'), onSelect: () => setFolder(folder.id) },
    {
      label: t('actions.connectAll'),
      disabled: all.length === 0,
      onSelect: () => void connectMany(all),
    },
    'separator',
    ...ctx.menu(folder),
  ];
  return (
    <>
      <ContextMenu>
        <ContextMenuTrigger asChild>
          <div
            {...drop.props}
            {...dragProps({ type: 'folder', id: folder.id })}
            role="row"
            data-testid="workspace-folder-row"
            onClick={() =>
              edit(ws.id, (w) => M.patchFolder(w, folder.id, { collapsed: !folder.collapsed }))
            }
            onDoubleClick={() => setFolder(folder.id)}
            className={cn(
              GRID,
              'group h-9 cursor-default border-b border-border/60 pr-2 text-[13px] hover:bg-surface-muted',
              drop.over && 'ring-1 ring-accent ring-inset',
            )}
          >
            <span
              className="flex min-w-0 items-center gap-1.5"
              style={{ paddingLeft: 8 + depth * 20 }}
            >
              {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
              {open ? (
                <FolderOpen size={14} style={{ color: ws.color }} />
              ) : (
                <Folder size={14} style={{ color: ws.color }} />
              )}
              <span className="min-w-0 truncate font-medium">{folder.name}</span>
              <span className="rounded-full bg-surface-muted px-1.5 text-[11px] text-muted-foreground">
                {ctx.filtering ? `${hits.length}/${all.length}` : all.length}
              </span>
            </span>
            <span className={KIND_CELL} />
            <span className={ENV_CELL} />
            <span className="flex items-center gap-2 text-xs text-muted-foreground">
              {connected > 0 && (
                <span className="text-success">{t('rollup.connected', { count: connected })}</span>
              )}
              {problems > 0 && (
                <span className="text-danger">{t('rollup.problems', { count: problems })}</span>
              )}
            </span>
            <span className={TABS_CELL} />
            <span
              className="flex justify-end opacity-0 group-hover:opacity-100"
              onClick={(e) => e.stopPropagation()}
            >
              <IconButton
                label={t('actions.connectAll')}
                icon={<PlugZap size={13} />}
                disabled={all.length === 0}
                onClick={() => void connectMany(all)}
              />
              <MoreMenu items={items} label={t('folder.menu')} />
            </span>
          </div>
        </ContextMenuTrigger>
        <ContextMenuContent>
          <ContextItems items={items} />
        </ContextMenuContent>
      </ContextMenu>
      {open && <FolderContents parent={folder.id} depth={depth + 1} ctx={ctx} />}
    </>
  );
}

function ConnectionRow({
  profile,
  depth,
  ctx,
}: {
  profile: ConnectionProfile;
  depth: number;
  ctx: RowContext;
}) {
  const { t } = useTranslation('workspaces');
  const { t: tc } = useTranslation('connections');
  const { ws } = ctx;
  const edit = useWorkspaces((s) => s.edit);
  const session = useSessionsStore((s) => s.sessionsByConnection[profile.id]);
  const error = useConnectErrors((s) => s.byConnection[profile.id]?.message);
  const status = ctx.status(profile.id);
  const color = connectionColor(profile);
  const folder = M.folderOf(ws, profile.id);
  const navigate = () => useNavigation.getState().navigate(profile.id);
  const newEditor =
    session && session.descriptor.capabilities.sql !== false
      ? () => openEditor(session)
      : undefined;
  const remove = () =>
    void confirmDialog({
      title: t('remove.title', { name: profile.name }),
      description: t('remove.body', { name: ws.name }),
      confirmLabel: t('remove.confirm'),
    }).then((ok) => ok && edit(ws.id, (w) => M.removeConnection(w, profile.id)));
  const items: MenuItemSpec[] = [
    { label: t('row.showInSidebar'), onSelect: navigate },
    session
      ? { label: t('row.disconnect'), onSelect: () => void disconnectMany([profile.id]) }
      : { label: t('row.connect'), onSelect: navigate },
    { label: t('row.newEditor'), disabled: !newEditor, onSelect: newEditor },
    'separator',
    {
      label: t('row.moveTo'),
      sub: moveTargets(
        ws,
        (target) => target !== folder,
        (target) => edit(ws.id, (w) => M.moveConnections(w, [profile.id], target)),
        t('allConnections'),
      ),
    },
    {
      label: t('row.newFolderWith'),
      disabled: !M.canNest(ws.folders, folder),
      onSelect: () =>
        newFolderDialog(ws, folder, edit, (id) =>
          edit(ws.id, (w) => M.moveConnections(w, [profile.id], id)),
        ),
    },
    'separator',
    { label: t('row.remove'), danger: true, onSelect: remove },
  ];
  const environment =
    profile.environmentLabel ||
    (profile.environment ? tc(`environment.${profile.environment}`) : '');
  const tabs = ctx.tabCount(profile.id);
  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>
        <div
          {...dragProps({ type: 'connections', ids: [profile.id] })}
          role="row"
          data-testid="workspace-connection-row"
          onDoubleClick={navigate}
          className={cn(
            GRID,
            'group h-10 cursor-default border-b border-border/60 pr-2 text-[13px] hover:bg-surface-muted',
          )}
          style={{ boxShadow: color ? `inset 3px 0 0 ${color}` : undefined }}
        >
          <span
            className="flex min-w-0 items-center gap-2"
            style={{ paddingLeft: 12 + depth * 20 }}
          >
            <span className="w-[24px] shrink-0 rounded bg-surface-muted text-center text-[9px] font-semibold leading-4 text-muted-foreground">
              {kindBadgeFor(profile.kind)}
            </span>
            <span className="min-w-0">
              <span className="block truncate font-medium">{profile.name}</span>
              <span className="block truncate text-[11px] text-muted-foreground">
                {[profile.host && `${profile.host}:${profile.port}`, profile.database]
                  .filter(Boolean)
                  .join(' / ')}
              </span>
            </span>
          </span>
          <span className={cn(KIND_CELL, 'truncate text-xs text-muted-foreground')}>
            {profile.kind}
          </span>
          <span className={cn(ENV_CELL, 'truncate text-xs')}>
            {environment && (
              <span
                className={cn(
                  'rounded px-1.5 py-0.5',
                  profile.environment === 'production'
                    ? 'bg-danger-soft text-danger'
                    : 'bg-surface-muted text-muted-foreground',
                )}
              >
                {environment}
              </span>
            )}
          </span>
          <span className="flex min-w-0 items-center gap-1.5 text-xs" title={error}>
            <span className={cn('size-2 shrink-0 rounded-full', STATUS_DOT[status])} />
            <span className={cn('truncate', PROBLEM_STATUSES.has(status) && 'text-danger')}>
              {t(`status.${status}`)}
            </span>
          </span>
          <span className={cn(TABS_CELL, 'text-xs text-muted-foreground')}>
            {tabs > 0 ? t('row.tabs', { count: tabs }) : ''}
          </span>
          <span className="flex justify-end opacity-0 group-hover:opacity-100 focus-within:opacity-100">
            {session ? (
              <>
                <IconButton
                  label={t('row.newEditor')}
                  icon={<SquareTerminal size={13} />}
                  disabled={!newEditor}
                  onClick={newEditor}
                />
                <IconButton
                  label={t('row.disconnect')}
                  icon={<Unplug size={13} />}
                  onClick={() => void disconnectMany([profile.id])}
                />
              </>
            ) : (
              <IconButton label={t('row.connect')} icon={<Plug size={13} />} onClick={navigate} />
            )}
            <IconButton label={t('row.remove')} icon={<X size={13} />} onClick={remove} />
          </span>
        </div>
      </ContextMenuTrigger>
      <ContextMenuContent>
        <ContextItems items={items} />
      </ContextMenuContent>
    </ContextMenu>
  );
}

/** Subfolders of `parent` (recursively) then the connections directly in it. */
function FolderContents({
  parent,
  depth,
  ctx,
}: {
  parent: string;
  depth: number;
  ctx: RowContext;
}) {
  const { t } = useTranslation('workspaces');
  const kids = M.childrenOf(ctx.ws.folders, parent);
  const rows = M.connectionsIn(ctx.ws, parent)
    .map((id) => ctx.profiles.get(id))
    .filter((p): p is ConnectionProfile => !!p && ctx.visible(p));
  return (
    <>
      {kids.map((f) => (
        <FolderRow key={f.id} folder={f} depth={depth} ctx={ctx} />
      ))}
      {rows.map((p) => (
        <ConnectionRow key={p.id} profile={p} depth={depth} ctx={ctx} />
      ))}
      {parent !== M.ROOT &&
        depth > 0 &&
        kids.length === 0 &&
        rows.length === 0 &&
        !ctx.filtering && (
          <p
            className="border-b border-border/60 py-2 text-xs italic text-muted-foreground"
            style={{ paddingLeft: 12 + depth * 20 }}
          >
            {t('folder.emptyHint')}
          </p>
        )}
    </>
  );
}

/** One breadcrumb step; dropping on it moves the dragged item into that folder. */
function Crumb({
  ws,
  id,
  onPick,
  children,
}: {
  ws: M.Workspace;
  id: string;
  onPick: (id: string) => void;
  children: ReactNode;
}) {
  const drop = useDropTarget(ws, id);
  return (
    <button
      type="button"
      {...drop.props}
      onClick={() => onPick(id)}
      className={cn('rounded px-1 hover:bg-surface-muted', drop.over && 'ring-1 ring-accent')}
    >
      {children}
    </button>
  );
}

function Breadcrumb({
  ws,
  menu,
}: {
  ws: M.Workspace;
  menu: (f: M.WorkspaceFolder) => MenuItemSpec[];
}) {
  const { t } = useTranslation('workspaces');
  const shown = useWorkspaces((s) => s.folder);
  const setFolder = useWorkspaces((s) => s.setFolder);
  const chain = M.pathTo(ws.folders, shown);
  const current = chain.at(-1);
  return (
    <div
      className="flex min-h-8 flex-wrap items-center gap-1 text-[13px]"
      aria-label={t('breadcrumb')}
    >
      <Crumb ws={ws} id={M.ROOT} onPick={setFolder}>
        {t('allConnections')}
      </Crumb>
      {chain.map((f) => (
        <span key={f.id} className="flex items-center gap-1">
          <ChevronRight size={12} className="text-muted-foreground" />
          <Crumb ws={ws} id={f.id} onPick={setFolder}>
            {f.name}
          </Crumb>
        </span>
      ))}
      {current && <MoreMenu items={menu(current)} label={t('folder.menu')} />}
      <span className="ml-1 text-xs text-muted-foreground">
        {t('connectionCount', { count: M.connectionsIn(ws, shown, true).length })}
      </span>
    </div>
  );
}

// ---- the page --------------------------------------------------------------------------------

/** Overview of the active workspace, shown in the first tab instead of the welcome page. */
export function WorkspaceView({ ws }: { ws: M.Workspace }) {
  const { t } = useTranslation('workspaces');
  const { data: snapshot } = useConnectionsSnapshot();
  const shown = useWorkspaces((s) => s.folder);
  const update = useWorkspaces((s) => s.update);
  const remove = useWorkspaces((s) => s.remove);
  const edit = useWorkspaces((s) => s.edit);
  const addToActive = useWorkspaces((s) => s.addToActive);
  // Select the record (stable), not a fresh key array: a new array per read loops forever.
  const sessionsByConnection = useSessionsStore((s) => s.sessionsByConnection);
  const openIds = useMemo(() => Object.keys(sessionsByConnection), [sessionsByConnection]);
  const tabs = useTabsStore((s) => s.tabs);
  const status = useConnectionStatus();
  const menu = useFolderMenu(ws);
  const [query, setQuery] = useState('');
  const [chip, setChip] = useState<Chip>('all');

  const profiles = useMemo(
    () => new Map((snapshot?.profiles ?? []).map((p) => [p.id, p])),
    [snapshot],
  );
  const members = ws.connections
    .map((id) => profiles.get(id))
    .filter((p): p is ConnectionProfile => !!p);
  const inShown = M.connectionsIn(ws, shown, true)
    .map((id) => profiles.get(id))
    .filter((p): p is ConnectionProfile => !!p);
  const needle = query.trim().toLowerCase();
  const textMatches = (p: ConnectionProfile) =>
    !needle ||
    needle
      .split(/\s+/)
      .every((w) => `${p.name} ${p.host} ${p.database ?? ''} ${p.kind}`.toLowerCase().includes(w));
  const visible = (p: ConnectionProfile) => textMatches(p) && chipMatches(chip, p, status(p.id));
  const filtering = !!needle || chip !== 'all';
  const visibleHere = inShown.filter(visible);
  const counts = Object.fromEntries(
    CHIPS.map((c) => [
      c,
      inShown.filter((p) => textMatches(p) && chipMatches(c, p, status(p.id))).length,
    ]),
  ) as Record<Chip, number>;
  const openNotIn = openIds.filter((id) => !ws.connections.includes(id));
  const tabCount = (id: Uuid) => tabs.filter((tab) => tab.payload?.connectionId === id).length;
  const ctx: RowContext = { ws, profiles, status, visible, filtering, menu, tabCount };
  const root = useDropTarget(ws, shown);

  const addItems: MenuItemSpec[] = [
    {
      label: t('actions.addSaved'),
      onSelect: () => openWorkspaceDialog({ type: 'add', workspaceId: ws.id, folder: shown }),
    },
    {
      label: t('actions.addOpen', { count: openNotIn.length }),
      disabled: openNotIn.length === 0,
      onSelect: () => addToActive(openNotIn, shown),
    },
    'separator',
    {
      label: shown ? t('folder.newSubfolder') : t('folder.new'),
      disabled: !M.canNest(ws.folders, shown),
      onSelect: () => newFolderDialog(ws, shown, edit),
    },
  ];
  const wsItems: MenuItemSpec[] = [
    {
      label: t('actions.rename'),
      onSelect: () =>
        openWorkspaceDialog({
          type: 'name',
          title: t('actions.renameTitle'),
          label: t('name'),
          initial: ws.name,
          submitLabel: t('common:actions.save'),
          onSubmit: (name) => update(ws.id, { name }),
        }),
    },
    'separator',
    {
      label: t('actions.delete'),
      danger: true,
      onSelect: () =>
        void confirmDialog({
          title: t('delete.title', { name: ws.name }),
          description: t('delete.body'),
          confirmLabel: t('common:actions.delete'),
          destructive: true,
        }).then((ok) => ok && remove(ws.id)),
    },
  ];

  return (
    <section
      className="@container h-full overflow-y-auto"
      aria-labelledby="workspace-title"
      data-testid="workspace-view"
    >
      <div className="w-full px-4 py-6 @2xl:px-8">
        <header className="flex flex-wrap items-start gap-3">
          <span
            className="mt-1.5 size-3 shrink-0 rounded-full"
            style={{ backgroundColor: ws.color }}
          />
          <div className="mr-auto min-w-0">
            <h1 id="workspace-title" className="truncate text-xl font-semibold">
              {ws.name}
            </h1>
            <p className="text-xs text-muted-foreground">
              {t('summary', { connections: members.length, folders: ws.folders.length })}
            </p>
          </div>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="primary" size="sm">
                <Plus size={13} /> {t('actions.addConnections')} <ChevronDown size={12} />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownItems items={addItems} />
            </DropdownMenuContent>
          </DropdownMenu>
          <Button
            size="sm"
            disabled={visibleHere.length === 0}
            onClick={() => void connectMany(visibleHere)}
            title={t('actions.connectAllHint')}
          >
            <PlugZap size={13} /> {t('actions.connectAll')}
          </Button>
          <Button
            size="sm"
            disabled={!visibleHere.some((p) => openIds.includes(p.id))}
            onClick={() => void disconnectMany(visibleHere.map((p) => p.id))}
          >
            <Unplug size={13} /> {t('actions.disconnectAll')}
          </Button>
          <MoreMenu items={wsItems} label={t('actions.more')} />
        </header>
        <div className="mt-3 flex items-center gap-3 text-xs text-muted-foreground">
          <span>{t('color')}</span>
          <ColorSwatches value={ws.color} onChange={(color) => update(ws.id, { color })} />
        </div>

        <div className="mt-6 grid grid-cols-1 gap-6 @3xl:grid-cols-[220px_minmax(0,1fr)]">
          <FolderTree ws={ws} menu={menu} />
          <div className="min-w-0">
            <Breadcrumb ws={ws} menu={menu} />
            <div className="mt-2 flex flex-wrap items-center gap-2">
              <Input
                type="search"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder={t('filter.placeholder')}
                aria-label={t('filter.placeholder')}
                className="h-7 w-64 text-xs"
              />
              <div
                className="flex flex-wrap gap-1"
                role="radiogroup"
                aria-label={t('filter.status')}
              >
                {CHIPS.map((c) => (
                  <button
                    key={c}
                    type="button"
                    role="radio"
                    aria-checked={chip === c}
                    onClick={() => setChip(c)}
                    className={cn(
                      'flex h-7 items-center gap-1.5 rounded-full border px-2.5 text-xs',
                      chip === c
                        ? 'border-accent bg-accent/15 text-foreground'
                        : 'border-border text-muted-foreground hover:bg-surface-muted',
                    )}
                  >
                    {t(`filter.${c}`)}
                    <span className="min-w-4 text-center tabular-nums">{counts[c]}</span>
                  </button>
                ))}
              </div>
            </div>

            <div
              {...root.props}
              role="table"
              aria-label={t('table')}
              className={cn(
                'mt-3 rounded-md border border-border',
                root.over && 'ring-1 ring-accent',
              )}
            >
              <div
                role="row"
                className={cn(
                  GRID,
                  'h-8 border-b border-border bg-surface-muted/50 pr-2 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground',
                )}
              >
                <span className="pl-3">{t('col.connection')}</span>
                <span className={KIND_CELL}>{t('col.kind')}</span>
                <span className={ENV_CELL}>{t('col.environment')}</span>
                <span>{t('col.status')}</span>
                <span className={TABS_CELL}>{t('col.tabs')}</span>
                <span />
              </div>
              {members.length === 0 ? (
                <div className="space-y-3 px-4 py-10 text-center">
                  <p className="text-sm text-muted-foreground">{t('emptyWorkspace')}</p>
                  <div className="flex justify-center gap-2">
                    <Button
                      size="sm"
                      variant="primary"
                      onClick={() =>
                        openWorkspaceDialog({ type: 'add', workspaceId: ws.id, folder: shown })
                      }
                    >
                      {t('actions.addSaved')}
                    </Button>
                    {openNotIn.length > 0 && (
                      <Button size="sm" onClick={() => addToActive(openNotIn, shown)}>
                        {t('actions.addOpen', { count: openNotIn.length })}
                      </Button>
                    )}
                  </div>
                </div>
              ) : (
                <>
                  <FolderContents parent={shown} depth={0} ctx={ctx} />
                  {filtering && visibleHere.length === 0 && (
                    <p className="px-4 py-6 text-center text-xs text-muted-foreground">
                      {t('filter.noMatch')}
                    </p>
                  )}
                </>
              )}
            </div>
          </div>
        </div>
      </div>
    </section>
  );
}
