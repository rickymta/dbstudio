import { ChevronRight, CircleAlert, Folder, FolderOpen, Lock } from 'lucide-react';
import type { KeyboardEvent, ReactNode } from 'react';
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

export interface RowActions {
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
  'flex h-[26px] cursor-default items-center gap-1 pr-2 text-[13px] outline-none ' +
  'hover:bg-surface-muted focus-visible:bg-accent/20 focus-visible:outline-none';

const indent = (depth: number) => 6 + depth * 12;

/** Up/down moves between rows marked `data-nav-row` inside the sidebar. */
function moveFocus(e: KeyboardEvent<HTMLElement>, delta: number) {
  const root = e.currentTarget.closest('[data-nav-root]');
  if (!root) return;
  const rows = Array.from(root.querySelectorAll<HTMLElement>('[data-nav-row]')).filter(
    (r) => !r.closest('[hidden]'),
  );
  const next = rows[rows.indexOf(e.currentTarget) + delta];
  if (next) {
    e.preventDefault();
    next.focus();
  }
}

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
      {children}
    </div>
  );
}
