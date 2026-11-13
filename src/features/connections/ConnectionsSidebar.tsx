import { useQueryClient } from '@tanstack/react-query';
import {
  ChevronsDownUp,
  Database,
  Download,
  Ellipsis,
  FolderPlus,
  Plus,
  Search,
  Upload,
  X,
} from 'lucide-react';
import type { DragEvent } from 'react';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ConfirmDialog } from '@/components/ui/ConfirmDialog';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/DropdownMenu';
import { EmptyState } from '@/components/ui/EmptyState';
import { IconButton } from '@/components/ui/IconButton';
import { Button } from '@/components/ui/Button';
import { Input } from '@/components/ui/Input';
import { Spinner } from '@/components/ui/Spinner';
import { toast } from '@/components/ui/toastStore';
import { cn } from '@/lib/cn';
import { pickFile, pickSavePath } from '@/lib/dialog';
import { ipc, toIpcError } from '@/lib/ipc';
import type { ConnectionGroup, ConnectionProfile, ObjectNode, Uuid } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { ObjectTree } from '@/features/explorer/ObjectTree';
import { treeHasMatch, useAnyTreeCacheVersion } from '@/features/explorer/queries';
import { nodeLabel } from '@/features/explorer/treeModel';
import { useRevealStore } from '@/features/explorer/revealStore';
import { ConnectionDialog } from './ConnectionDialog';
import {
  buildConnectionsTree,
  filterConnectionsTree,
  groupPathOptions,
  needsPassword,
  type GroupNode,
} from './connectionsTree';
import { dropPosition, planMove, type DragItem, type DropTarget } from './dragModel';
import { describeError } from './errors';
import { GroupDialog } from './GroupDialog';
import { PasswordDialog } from './PasswordDialog';
import { connectionsQueryKey, useConnectionsSnapshot } from './queries';
import { ServerSearch } from './ServerSearch';
import { useSearchableSessions } from './serverSearchModel';
import { GroupRow, ProfileRow, type DndApi, type RowActions } from './SidebarRows';

const COLLAPSED_KEY = 'dbstudio.connections.collapsedGroups';
const JSON_FILTER = [{ name: 'JSON', extensions: ['json'] }];

function loadCollapsed(): Set<Uuid> {
  try {
    const raw = localStorage.getItem(COLLAPSED_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : [];
    return new Set(Array.isArray(parsed) ? parsed.filter((x) => typeof x === 'string') : []);
  } catch {
    return new Set();
  }
}

function saveCollapsed(ids: Set<Uuid>) {
  try {
    localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...ids]));
  } catch {
    // storage unavailable
  }
}

type Modal =
  | { type: 'connection'; profile?: ConnectionProfile; groupId?: Uuid | null }
  | { type: 'group'; mode: 'create'; parentId: Uuid | null }
  | { type: 'group'; mode: 'rename'; group: ConnectionGroup }
  | { type: 'deleteProfile'; profile: ConnectionProfile }
  | { type: 'deleteGroup'; group: ConnectionGroup }
  | { type: 'password'; profile: ConnectionProfile };

/** Left sidebar: connection groups and profiles, each connected profile with its object tree. */
export function ConnectionsSidebar() {
  const { t } = useTranslation('connections');
  const qc = useQueryClient();
  const query = useConnectionsSnapshot();
  const snapshot = query.data;

  const [filter, setFilter] = useState('');
  const [collapsed, setCollapsed] = useState<Set<Uuid>>(loadCollapsed);
  const [expandedProfiles, setExpandedProfiles] = useState<Set<Uuid>>(() => new Set());
  const [errors, setErrors] = useState<Record<Uuid, string>>({});
  const [modal, setModal] = useState<Modal | null>(null);

  const filtering = filter.trim().length > 0;
  const asideRef = useRef<HTMLElement>(null);
  const debouncedFilter = useDebounced(filter.trim(), 300);
  const searchableSessions = useSearchableSessions();
  const reveal = useRevealStore((s) => s.reveal);
  useAnyTreeCacheVersion(filtering);

  const { t: te } = useTranslation('explorer');
  const explorerLabel = useCallback((n: ObjectNode) => nodeLabel(te, n), [te]);

  const tree = useMemo(() => {
    if (!snapshot) return null;
    return buildConnectionsTree(snapshot);
  }, [snapshot]);

  const groupOptions = useMemo(() => (snapshot ? groupPathOptions(snapshot) : []), [snapshot]);

  const refresh = useCallback(() => qc.invalidateQueries({ queryKey: connectionsQueryKey }), [qc]);

  /** Run a backend mutation, refresh the list and toast failures. */
  const run = useCallback(
    async (fn: () => Promise<unknown>, success?: string) => {
      try {
        await fn();
        await refresh();
        if (success) toast.success(success);
      } catch (e) {
        toast.error(describeError(t, e));
        await refresh();
      }
    },
    [refresh, t],
  );

  const setProfileExpanded = (id: Uuid, expanded: boolean) =>
    setExpandedProfiles((prev) => {
      const next = new Set(prev);
      if (expanded) next.add(id);
      else next.delete(id);
      return next;
    });

  const connect = useCallback(
    async (profile: ConnectionProfile, password?: string) => {
      const sessions = useSessionsStore.getState();
      if (sessions.sessionsByConnection[profile.id]) {
        setProfileExpanded(profile.id, true);
        return;
      }
      if (password === undefined && needsPassword(profile)) {
        setModal({ type: 'password', profile });
        return;
      }
      setErrors((e) => {
        const rest = { ...e };
        delete rest[profile.id];
        return rest;
      });
      try {
        await sessions.open(profile.id, password ?? null);
        setProfileExpanded(profile.id, true);
      } catch (e) {
        const error = toIpcError(e);
        if (error.kind === 'cancelled') return; // disconnected while connecting
        const message = describeError(t, error);
        setErrors((prev) => ({ ...prev, [profile.id]: message }));
        toast.error(`${profile.name}: ${message}`);
      }
    },
    [t],
  );

  // Drag and drop (CONN-02). The dragged item lives in a ref because `dragover` cannot read
  // `dataTransfer` data.
  const dragRef = useRef<DragItem | null>(null);
  // A saved move plan is several writes in a row; no new drag may start before it finishes.
  const movingRef = useRef(false);
  const [moving, setMoving] = useState(false);
  const [hint, setHint] = useState<DropTarget | null>(null);

  const targetFor = (e: DragEvent<HTMLElement>, row: DragItem): DropTarget | null => {
    const dragged = dragRef.current;
    if (!dragged) return null;
    const rect = e.currentTarget.getBoundingClientRect();
    const position = dropPosition(dragged.type, row.type, (e.clientY - rect.top) / rect.height);
    return position ? { ...row, position } : null;
  };

  const sameHint = (a: DropTarget | null, b: DropTarget | null) =>
    JSON.stringify(a) === JSON.stringify(b);

  const hover = (e: DragEvent<HTMLElement>, target: DropTarget | null, onRow = false) => {
    const dragged = dragRef.current;
    if (
      !dragged ||
      !target ||
      !snapshot ||
      movingRef.current ||
      !planMove(snapshot, dragged, target)
    ) {
      // An invalid drop on a row must not fall through to the "move to root" container.
      if (onRow) e.stopPropagation();
      setHint(null);
      return;
    }
    e.preventDefault();
    e.stopPropagation();
    e.dataTransfer.dropEffect = 'move';
    setHint((prev) => (sameHint(prev, target) ? prev : target));
  };

  const endDrag = () => {
    dragRef.current = null;
    setHint(null);
  };

  const dropOn = (e: DragEvent<HTMLElement>, target: DropTarget | null) => {
    const dragged = dragRef.current;
    const plan = dragged && target && snapshot ? planMove(snapshot, dragged, target) : null;
    endDrag();
    if (target && target.type !== 'root') e.stopPropagation();
    if (!plan) return;
    e.preventDefault();
    e.stopPropagation();
    if (movingRef.current) return;
    movingRef.current = true;
    setMoving(true);
    // `run` toasts a failure and reloads the list, so a half-applied plan is shown as it is.
    void run(async () => {
      for (const p of plan.profiles) await ipc.connectionSave(p, { action: 'keep' });
      for (const g of plan.groups) await ipc.groupSave(g);
    }).finally(() => {
      movingRef.current = false;
      setMoving(false);
    });
  };

  const dnd: DndApi = {
    hint,
    busy: moving,
    start: (item, e) => {
      if (movingRef.current) {
        e.preventDefault();
        return;
      }
      dragRef.current = item;
      e.dataTransfer.effectAllowed = 'move';
      e.dataTransfer.setData('text/plain', item.id);
    },
    end: endDrag,
    over: (e, row) => hover(e, targetFor(e, row), true),
    drop: (e, row) => dropOn(e, targetFor(e, row)),
  };

  // Hovering a collapsed group while dragging opens it after a short delay.
  const hintedGroup = hint && hint.type === 'group' && hint.position === 'into' ? hint.id : null;
  useEffect(() => {
    if (!hintedGroup || !collapsed.has(hintedGroup)) return;
    const id = setTimeout(() => {
      setCollapsed((prev) => {
        const next = new Set(prev);
        next.delete(hintedGroup);
        saveCollapsed(next);
        return next;
      });
    }, 600);
    return () => clearTimeout(id);
  }, [hintedGroup, collapsed]);

  const actions: RowActions = {
    dnd,
    connect: (p) => void connect(p),
    disconnect: (p) =>
      void useSessionsStore
        .getState()
        .requestClose(p.id)
        .then((closed) => {
          if (closed) setProfileExpanded(p.id, false);
        }),
    edit: (profile) => setModal({ type: 'connection', profile }),
    duplicate: (p) => void run(() => ipc.connectionDuplicate(p.id), t('toast.duplicated')),
    moveTo: (p, groupId) =>
      void run(() => ipc.connectionSave({ ...p, groupId }, { action: 'keep' })),
    remove: (profile) => setModal({ type: 'deleteProfile', profile }),
    toggleProfile: (id) => setProfileExpanded(id, !expandedProfiles.has(id)),
    toggleGroup: (id) =>
      setCollapsed((prev) => {
        const next = new Set(prev);
        if (next.has(id)) next.delete(id);
        else next.add(id);
        saveCollapsed(next);
        return next;
      }),
    newInGroup: (groupId) => {
      setCollapsed((prev) => {
        const next = new Set(prev);
        next.delete(groupId);
        saveCollapsed(next);
        return next;
      });
      setModal({ type: 'connection', groupId });
    },
    newSubgroup: (parentId) => setModal({ type: 'group', mode: 'create', parentId }),
    renameGroup: (group) => setModal({ type: 'group', mode: 'rename', group }),
    removeGroup: (group) => setModal({ type: 'deleteGroup', group }),
  };

  const showInTree = (connectionId: Uuid, node: ObjectNode) => {
    // Clear the filter so the node is not filtered out, then let the tree expand down to it.
    setFilter('');
    setProfileExpanded(connectionId, true);
    reveal(connectionId, node.path);
  };

  const collapseAll = () => {
    const ids = new Set(snapshot?.groups.map((g) => g.id) ?? []);
    setCollapsed(ids);
    saveCollapsed(ids);
    setExpandedProfiles(new Set());
  };

  const importConnections = async () => {
    const path = await pickFile({ title: t('toolbar.import'), filters: JSON_FILTER });
    if (!path) return;
    await run(() => ipc.connectionsImport(path), t('toast.imported'));
  };

  const exportConnections = async () => {
    const path = await pickSavePath({
      title: t('toolbar.export'),
      defaultPath: 'dbstudio-connections.json',
      filters: JSON_FILTER,
    });
    if (!path) return;
    try {
      const count = await ipc.connectionsExport(path);
      toast.success(t('toast.exported', { count }));
    } catch (e) {
      toast.error(describeError(t, e));
    }
  };

  const sessionsByConnection = useSessionsStore((s) => s.sessionsByConnection);
  const profileMatches = (p: ConnectionProfile): boolean => {
    const f = filter.trim().toLowerCase();
    if (p.name.toLowerCase().includes(f)) return true;
    const session = sessionsByConnection[p.id];
    return session !== undefined && treeHasMatch(qc, session.sessionId, f, explorerLabel);
  };

  const visible = tree ? filterConnectionsTree(tree, filter, profileMatches) : null;
  const isEmpty = !!snapshot && snapshot.groups.length === 0 && snapshot.profiles.length === 0;

  const renderProfile = (p: ConnectionProfile, depth: number) => {
    const session = sessionsByConnection[p.id];
    const expanded = expandedProfiles.has(p.id);
    // While filtering, connected profiles show their (filtered) tree if it has matches.
    const treeVisible = session
      ? filtering
        ? treeHasMatch(qc, session.sessionId, filter.trim().toLowerCase(), explorerLabel)
        : expanded
      : false;
    return (
      <ProfileRow
        key={p.id}
        profile={p}
        depth={depth}
        expanded={treeVisible}
        error={errors[p.id]}
        groupOptions={groupOptions}
        actions={actions}
      >
        {session && (
          <div
            role="group"
            hidden={!treeVisible}
            className="pb-0.5"
            style={{ paddingLeft: 6 + depth * 12 + 8 }}
          >
            <ObjectTree connectionId={p.id} filter={filter} nested />
          </div>
        )}
      </ProfileRow>
    );
  };

  const renderGroup = (node: GroupNode, depth: number) => {
    const open = filtering || !collapsed.has(node.group.id);
    return (
      <GroupRow key={node.group.id} group={node.group} depth={depth} open={open} actions={actions}>
        {node.groups.map((g) => renderGroup(g, depth + 1))}
        {node.profiles.map((p) => renderProfile(p, depth + 1))}
      </GroupRow>
    );
  };

  const deleteProfile = (profile: ConnectionProfile) =>
    run(async () => {
      if (useSessionsStore.getState().sessionsByConnection[profile.id]) {
        // A tab with unfinished work may veto: keep the profile (and its session) then.
        if (!(await useSessionsStore.getState().requestClose(profile.id))) return;
      }
      setProfileExpanded(profile.id, false);
      await ipc.connectionDelete(profile.id);
    });

  const deleteGroup = (group: ConnectionGroup) =>
    run(async () => {
      await ipc.groupDelete(group.id);
      setCollapsed((prev) => {
        const next = new Set(prev);
        next.delete(group.id);
        saveCollapsed(next);
        return next;
      });
    });

  const createGroup = (name: string, parentId: Uuid | null) => {
    const siblings = snapshot?.groups.filter((g) => (g.parentId ?? null) === parentId).length ?? 0;
    void run(() => ipc.groupSave({ id: crypto.randomUUID(), name, parentId, sortOrder: siblings }));
  };

  return (
    <aside
      className="flex h-full flex-col bg-surface"
      aria-label={t('sidebar.title')}
      data-nav-root
      ref={asideRef}
    >
      <header className="flex h-9 shrink-0 items-center gap-1 border-b border-border pr-2 pl-3">
        <h2 className="mr-auto text-xs font-semibold uppercase tracking-wide text-muted-foreground">
          {t('sidebar.title')}
        </h2>
        <IconButton
          label={t('toolbar.newConnection')}
          icon={<Plus size={14} />}
          onClick={() => setModal({ type: 'connection' })}
        />
        <IconButton
          label={t('toolbar.newGroup')}
          icon={<FolderPlus size={14} />}
          onClick={() => setModal({ type: 'group', mode: 'create', parentId: null })}
        />
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              size="icon"
              variant="ghost"
              aria-label={t('toolbar.more')}
              title={t('toolbar.more')}
            >
              <Ellipsis size={14} />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem onSelect={() => void importConnections()}>
              <Upload size={13} />
              {t('toolbar.import')}
            </DropdownMenuItem>
            <DropdownMenuItem onSelect={() => void exportConnections()}>
              <Download size={13} />
              {t('toolbar.export')}
            </DropdownMenuItem>
            <DropdownMenuItem onSelect={collapseAll}>
              <ChevronsDownUp size={13} />
              {t('toolbar.collapseAll')}
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </header>

      <div className="relative shrink-0 border-b border-border px-2 py-1.5">
        <Search
          size={13}
          aria-hidden
          className="pointer-events-none absolute top-1/2 left-4 -translate-y-1/2 text-muted-foreground"
        />
        <Input
          type="search"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          data-search-input
          onKeyDown={(e) => {
            if (e.key === 'Escape') setFilter('');
            else if (e.key === 'ArrowDown') {
              const first = asideRef.current?.querySelector<HTMLElement>('[data-nav-row]');
              if (first) {
                e.preventDefault();
                first.focus();
              }
            }
          }}
          placeholder={t('sidebar.search')}
          aria-label={t('sidebar.search')}
          className="h-6 pr-6 pl-6 text-xs [&::-webkit-search-cancel-button]:hidden"
        />
        {filter && (
          <button
            type="button"
            aria-label={t('sidebar.clearSearch')}
            onClick={() => setFilter('')}
            className="absolute top-1/2 right-3.5 -translate-y-1/2 rounded p-0.5 text-muted-foreground hover:text-foreground"
          >
            <X size={12} />
          </button>
        )}
      </div>

      <div
        role="tree"
        onDragOver={(e) => isRootDropZone(e) && hover(e, { type: 'root' })}
        onDrop={(e) => isRootDropZone(e) && dropOn(e, { type: 'root' })}
        aria-label={t('sidebar.title')}
        className={cn(
          'min-h-0 flex-1 overflow-y-auto overflow-x-hidden py-1',
          hint?.type === 'root' && 'bg-accent/10',
        )}
      >
        {query.isPending && (
          <div className="flex items-center justify-center gap-2 p-4 text-xs text-muted-foreground">
            <Spinner size={14} />
            {t('sidebar.loading')}
          </div>
        )}
        {query.isError && (
          <EmptyState
            title={t('sidebar.loadError')}
            description={toIpcError(query.error).message}
            action={<Button onClick={() => void query.refetch()}>{t('row.retry')}</Button>}
          />
        )}
        {isEmpty && (
          <EmptyState
            icon={<Database size={28} aria-hidden />}
            title={t('sidebar.empty')}
            description={t('sidebar.emptyHint')}
            action={
              <Button variant="primary" onClick={() => setModal({ type: 'connection' })}>
                <Plus size={14} />
                {t('toolbar.newConnection')}
              </Button>
            }
          />
        )}
        {visible && !isEmpty && (
          <>
            {visible.groups.map((g) => renderGroup(g, 0))}
            {visible.profiles.map((p) => renderProfile(p, 0))}
            {filtering && visible.groups.length === 0 && visible.profiles.length === 0 && (
              <p className="px-3 py-4 text-center text-xs text-muted-foreground">
                {t('sidebar.noResults')}
              </p>
            )}
          </>
        )}
        <ServerSearch
          query={debouncedFilter}
          sessions={searchableSessions}
          onShowInTree={showInTree}
        />
        {/* Empty area at the end of the list: dropping here moves an item to the top level. */}
        <div data-drop-root className="h-8" />
      </div>

      <ConnectionDialog
        open={modal?.type === 'connection'}
        onOpenChange={(o) => !o && setModal(null)}
        profile={modal?.type === 'connection' ? modal.profile : undefined}
        groupId={modal?.type === 'connection' ? modal.groupId : undefined}
      />
      <GroupDialog
        open={modal?.type === 'group'}
        onOpenChange={(o) => !o && setModal(null)}
        mode={modal?.type === 'group' ? modal.mode : 'create'}
        initialName={modal?.type === 'group' && modal.mode === 'rename' ? modal.group.name : ''}
        onSubmit={(name) => {
          if (modal?.type !== 'group') return;
          if (modal.mode === 'create') createGroup(name, modal.parentId);
          else void run(() => ipc.groupSave({ ...modal.group, name }));
        }}
      />
      <PasswordDialog
        open={modal?.type === 'password'}
        onOpenChange={(o) => !o && setModal(null)}
        connectionName={modal?.type === 'password' ? modal.profile.name : ''}
        onSubmit={(password) => {
          if (modal?.type === 'password') void connect(modal.profile, password);
        }}
      />
      <ConfirmDialog
        open={modal?.type === 'deleteProfile'}
        onOpenChange={(o) => !o && setModal(null)}
        title={t('confirm.deleteProfileTitle', {
          name: modal?.type === 'deleteProfile' ? modal.profile.name : '',
        })}
        description={t('confirm.deleteProfileBody')}
        confirmLabel={t('menu.delete')}
        destructive
        onConfirm={() => modal?.type === 'deleteProfile' && void deleteProfile(modal.profile)}
      />
      <ConfirmDialog
        open={modal?.type === 'deleteGroup'}
        onOpenChange={(o) => !o && setModal(null)}
        title={t('confirm.deleteGroupTitle', {
          name: modal?.type === 'deleteGroup' ? modal.group.name : '',
        })}
        description={t('confirm.deleteGroupBody')}
        confirmLabel={t('menu.delete')}
        destructive
        onConfirm={() => modal?.type === 'deleteGroup' && void deleteGroup(modal.group)}
      />
    </aside>
  );
}

/** Top-level drops are only accepted on the empty area, not on a tree or a result bubbling up. */
function isRootDropZone(e: DragEvent<HTMLElement>): boolean {
  const target = e.target as HTMLElement;
  return target === e.currentTarget || target.dataset.dropRoot !== undefined;
}

/** `value`, but only after it stopped changing for `delayMs`. */
function useDebounced<T>(value: T, delayMs: number): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const id = setTimeout(() => setDebounced(value), delayMs);
    return () => clearTimeout(id);
  }, [value, delayMs]);
  return debounced;
}
