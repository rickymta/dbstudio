import { ChevronRight, CircleAlert, Folder, FolderOpen, Lock } from 'lucide-react';
import type { DragEvent, KeyboardEvent, ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
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
import { Spinner } from '@/components/ui/Spinner';
import { cn } from '@/lib/cn';
import type { ConnectionGroup, ConnectionProfile, Uuid } from '@/lib/types';
import { useIsOpening, useSession } from '@/stores/sessions';
import { kindBadgeFor } from './kindConfig';
import { connectionColor } from './queries';
import type { DragItem, DropTarget } from './dragModel';
import { moveFocus } from './rowNav';
import { SlowConnectHint } from './SlowConnectHint';

/** Drag-and-drop plumbing handed to every row by the sidebar (CONN-02). */
export interface DndApi {
  hint: DropTarget | null;
  /** A move is being saved: dragging is disabled until it finishes. */
  busy: boolean;
  start: (item: DragItem, e: DragEvent<HTMLElement>) => void;
  end: () => void;
  over: (e: DragEvent<HTMLElement>, row: DragItem) => void;
  drop: (e: DragEvent<HTMLElement>, row: DragItem) => void;
}

/** Spread on a row element: makes it draggable and a drop target. */
function dndProps(dnd: DndApi, row: DragItem) {
  return {
    draggable: !dnd.busy,
    onDragStart: (e: DragEvent<HTMLElement>) => dnd.start(row, e),
    onDragEnd: dnd.end,
    onDragOver: (e: DragEvent<HTMLElement>) => dnd.over(e, row),
    onDrop: (e: DragEvent<HTMLElement>) => dnd.drop(e, row),
  };
}

/** Insertion line / highlight for the current drop hint. */
function DropIndicator({ dnd, row }: { dnd: DndApi; row: DragItem }) {
  const h = dnd.hint;
  if (!h || h.type === 'root' || h.type !== row.type || h.id !== row.id) return null;
  if (h.position === 'into') {
    return (
      <span
        aria-hidden
        data-testid="drop-into"
        className="pointer-events-none absolute inset-0 rounded-sm bg-accent/20 ring-1 ring-accent"
      />
    );
  }
  return (
    <span
      aria-hidden
      data-testid={`drop-${h.position}`}
      className={cn(
        'pointer-events-none absolute inset-x-0 h-0.5 bg-accent',
        h.position === 'before' ? 'top-0' : 'bottom-0',
      )}
    />
  );
}

export interface RowActions {
  dnd: DndApi;
  connect: (p: ConnectionProfile) => void;
  disconnect: (p: ConnectionProfile) => void;
  edit: (p: ConnectionProfile) => void;
  duplicate: (p: ConnectionProfile) => void;
  moveTo: (p: ConnectionProfile, groupId: Uuid | null) => void;
  remove: (p: ConnectionProfile) => void;
  toggleProfile: (id: Uuid) => void;
  toggleGroup: (id: Uuid) => void;
  newInGroup: (groupId: Uuid) => void;
  newSubgroup: (groupId: Uuid) => void;
  renameGroup: (g: ConnectionGroup) => void;
  removeGroup: (g: ConnectionGroup) => void;
}

const ROW_CLASS =
  'relative flex h-[26px] cursor-default items-center gap-1 pr-2 text-[13px] outline-none ' +
  'hover:bg-surface-muted focus-visible:bg-accent/20 focus-visible:outline-none';

const indent = (depth: number) => 6 + depth * 12;

export function GroupRow({
  group,
  depth,
  open,
  actions,
  children,
}: {
  group: ConnectionGroup;
  depth: number;
  open: boolean;
  actions: RowActions;
  children?: ReactNode;
}) {
  const { t } = useTranslation('connections');
  const Icon = open ? FolderOpen : Folder;
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.target !== e.currentTarget) return;
    if (e.key === 'ArrowDown') moveFocus(e, 1);
    else if (e.key === 'ArrowUp') moveFocus(e, -1);
    else if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      actions.toggleGroup(group.id);
    } else if (e.key === 'ArrowRight' && !open) actions.toggleGroup(group.id);
    else if (e.key === 'ArrowLeft' && open) actions.toggleGroup(group.id);
  };
  return (
    <div role="none">
      <ContextMenu>
        <ContextMenuTrigger asChild>
          <div
            role="treeitem"
            aria-expanded={open}
            aria-label={group.name}
            tabIndex={0}
            data-nav-row
            data-testid="group-row"
            {...dndProps(actions.dnd, { type: 'group', id: group.id })}
            title={group.name}
            onClick={() => actions.toggleGroup(group.id)}
            onKeyDown={onKeyDown}
            style={{ paddingLeft: indent(depth) }}
            className={ROW_CLASS}
          >
            <ChevronRight
              size={12}
              className={cn(
                'shrink-0 text-muted-foreground transition-transform',
                open && 'rotate-90',
              )}
            />
            <Icon size={14} className="shrink-0 text-muted-foreground" aria-hidden />
            <span className="min-w-0 truncate font-medium">{group.name}</span>
            <DropIndicator dnd={actions.dnd} row={{ type: 'group', id: group.id }} />
          </div>
        </ContextMenuTrigger>
        <ContextMenuContent>
          <ContextMenuItem onSelect={() => actions.newInGroup(group.id)}>
            {t('menu.newConnectionInGroup')}
          </ContextMenuItem>
          <ContextMenuItem onSelect={() => actions.newSubgroup(group.id)}>
            {t('menu.newSubgroup')}
          </ContextMenuItem>
          <ContextMenuSeparator />
          <ContextMenuItem onSelect={() => actions.renameGroup(group)}>
            {t('menu.rename')}
          </ContextMenuItem>
          <ContextMenuItem variant="danger" onSelect={() => actions.removeGroup(group)}>
            {t('menu.delete')}
          </ContextMenuItem>
        </ContextMenuContent>
      </ContextMenu>
      {open && <div role="group">{children}</div>}
    </div>
  );
}

export function ProfileRow({
  profile,
  depth,
  expanded,
  error,
  groupOptions,
  actions,
  children,
}: {
  profile: ConnectionProfile;
  depth: number;
  expanded: boolean;
  /** Last connect error message, shown as an error state with a retry button. */
  error?: string;
  groupOptions: { id: Uuid; label: string }[];
  actions: RowActions;
  children?: ReactNode;
}) {
  const { t } = useTranslation('connections');
  const session = useSession(profile.id);
  const opening = useIsOpening(profile.id);
  const connected = session !== undefined;
  const color = connectionColor(profile);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.target !== e.currentTarget) return;
    if (e.key === 'ArrowDown') moveFocus(e, 1);
    else if (e.key === 'ArrowUp') moveFocus(e, -1);
    else if (e.key === 'Enter') {
      e.preventDefault();
      actions.connect(profile);
    } else if (e.key === 'ArrowRight' && connected && !expanded) actions.toggleProfile(profile.id);
    else if (e.key === 'ArrowLeft' && connected && expanded) actions.toggleProfile(profile.id);
  };

  const status = opening ? (
    <Spinner size={12} />
  ) : error ? (
    <button
      type="button"
      aria-label={t('row.retry')}
      title={`${error} - ${t('row.retry')}`}
      onClick={(e) => {
        e.stopPropagation();
        actions.connect(profile);
      }}
      onDoubleClick={(e) => e.stopPropagation()}
      className="flex items-center rounded p-0.5 text-danger hover:bg-danger-soft"
    >
      <CircleAlert size={13} />
    </button>
  ) : connected ? (
    <span
      role="img"
      aria-label={t('row.connected')}
      title={t('row.connected')}
      className="size-2 rounded-full bg-success"
    />
  ) : null;

  const subtitle = [profile.host, profile.database].filter(Boolean).join(' / ');

  return (
    <div role="none">
      <ContextMenu>
        <ContextMenuTrigger asChild>
          <div
            role="treeitem"
            aria-expanded={connected ? expanded : undefined}
            aria-label={profile.name}
            tabIndex={0}
            data-nav-row
            data-testid="profile-row"
            {...dndProps(actions.dnd, { type: 'profile', id: profile.id })}
            title={`${profile.name}\n${subtitle}`}
            onClick={() => connected && actions.toggleProfile(profile.id)}
            onDoubleClick={() => actions.connect(profile)}
            onKeyDown={onKeyDown}
            style={{
              paddingLeft: indent(depth),
              boxShadow: color ? `inset 3px 0 0 ${color}` : undefined,
            }}
            className={ROW_CLASS}
          >
            <span className="flex size-3 shrink-0 items-center justify-center">
              {connected && (
                <ChevronRight
                  size={12}
                  className={cn(
                    'text-muted-foreground transition-transform',
                    expanded && 'rotate-90',
                  )}
                />
              )}
            </span>
            <span
              aria-hidden
              className="w-[22px] shrink-0 rounded bg-surface-muted py-px text-center text-[9px] font-semibold leading-3 text-muted-foreground"
            >
              {kindBadgeFor(profile.kind)}
            </span>
            <span className="min-w-0 truncate">{profile.name}</span>
            {profile.readOnly && (
              <Lock
                size={10}
                aria-label={t('row.readOnly')}
                className="shrink-0 text-muted-foreground"
              />
            )}
            <span className="ml-auto flex shrink-0 items-center pl-1">{status}</span>
            <DropIndicator dnd={actions.dnd} row={{ type: 'profile', id: profile.id }} />
          </div>
        </ContextMenuTrigger>
        <ContextMenuContent>
          {connected ? (
            <ContextMenuItem onSelect={() => actions.disconnect(profile)}>
              {t('menu.disconnect')}
            </ContextMenuItem>
          ) : (
            <ContextMenuItem onSelect={() => actions.connect(profile)}>
              {t('menu.connect')}
            </ContextMenuItem>
          )}
          <ContextMenuSeparator />
          <ContextMenuItem onSelect={() => actions.edit(profile)}>{t('menu.edit')}</ContextMenuItem>
          <ContextMenuItem onSelect={() => actions.duplicate(profile)}>
            {t('menu.duplicate')}
          </ContextMenuItem>
          <ContextMenuSub>
            <ContextMenuSubTrigger>{t('menu.moveToGroup')}</ContextMenuSubTrigger>
            <ContextMenuSubContent>
              <ContextMenuItem
                disabled={!profile.groupId}
                onSelect={() => actions.moveTo(profile, null)}
              >
                {t('menu.noGroup')}
              </ContextMenuItem>
              {groupOptions.map((g) => (
                <ContextMenuItem
                  key={g.id}
                  disabled={profile.groupId === g.id}
                  onSelect={() => actions.moveTo(profile, g.id)}
                >
                  {g.label}
                </ContextMenuItem>
              ))}
            </ContextMenuSubContent>
          </ContextMenuSub>
          <ContextMenuSeparator />
          <ContextMenuItem variant="danger" onSelect={() => actions.remove(profile)}>
            {t('menu.delete')}
          </ContextMenuItem>
        </ContextMenuContent>
      </ContextMenu>
      <SlowConnectHint
        active={opening}
        keychain={profile.savePassword && profile.hasPassword}
        className="px-2 py-1 text-xs text-muted-foreground"
      />
      {children}
    </div>
  );
}
