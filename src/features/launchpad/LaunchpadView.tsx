import {
  ChevronDown,
  ChevronRight,
  CircleDot,
  FileWarning,
  Loader,
  PencilLine,
  Rocket,
  Star,
  StarOff,
} from 'lucide-react';
import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from '@/components/ui/DropdownMenu';
import { Input } from '@/components/ui/Input';
import { toast } from '@/components/ui/toastStore';
import { useConnectErrors } from '@/features/connections/connectErrors';
import { kindBadgeFor } from '@/features/connections/kindConfig';
import { useConnectionsSnapshot } from '@/features/connections/queries';
import { cancelRun, endTransaction } from '@/features/editor/editorRunner';
import { openEditor } from '@/features/editor/open';
import { isActive, jobFraction, sortedJobs, useJobsStore } from '@/features/jobs/jobsStore';
import { useNavigation, type SavedObject } from '@/features/workspace/navigation';
import {
  disconnectMany,
  PROBLEM_STATUSES,
  useConnectionStatus,
  type ConnectionStatus,
} from '@/features/workspaces/connectionStatus';
import { activeWorkspace, useWorkspaces } from '@/features/workspaces/store';
import { cn } from '@/lib/cn';
import { ipc, toIpcError } from '@/lib/ipc';
import type { ConnectionProfile, JobInfo, Uuid } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { NO_SCOPE, useTabsStore, type TabTitle } from '@/stores/tabs';
import { useWork } from './hooks';
import { ago, inScope, WORK_ORDER, type WorkItem, type WorkKind } from './model';
import { useLaunchpad, type LaunchpadSection } from './store';

const SECTIONS: LaunchpadSection[] = ['work', 'connections', 'objects', 'jobs'];

const WORK_ICON: Record<WorkKind, ReactNode> = {
  transaction: <CircleDot size={14} className="text-warning" />,
  running: <Loader size={14} className="animate-spin text-accent" />,
  pendingEdits: <PencilLine size={14} className="text-accent" />,
  unsavedFile: <FileWarning size={14} className="text-muted-foreground" />,
};

/** Show a tab, switching to the workspace holding it first. */
function goToTab(tabId: string, scope: string) {
  const ws = useWorkspaces.getState();
  if ((ws.active || NO_SCOPE) !== scope) ws.activate(scope);
  useTabsStore.getState().activate(tabId);
  useLaunchpad.getState().setOpen(false);
}

function useTitle() {
  const { t } = useTranslation();
  return (title: TabTitle) =>
    typeof title === 'string' ? title : t(title.key, { ns: title.ns ?? 'common' });
}

// ---- shared pieces ---------------------------------------------------------------------------

/** A row of a section; J/K and the arrows move between rows, Enter runs `onOpen`. */
function Row({
  onOpen,
  children,
  testId,
}: {
  onOpen?: () => void;
  children: ReactNode;
  testId?: string;
}) {
  return (
    <div
      role="row"
      tabIndex={0}
      data-lp-row
      data-testid={testId}
      onDoubleClick={onOpen}
      onKeyDown={(e) => {
        if (e.key === 'Enter' && e.target === e.currentTarget && onOpen) {
          e.preventDefault();
          onOpen();
        }
      }}
      className="group grid min-h-11 grid-cols-[minmax(0,1fr)_minmax(0,0.6fr)_auto] items-center gap-3 border-b border-border/60 px-3 text-[13px] outline-none hover:bg-surface-muted focus-visible:bg-accent/10"
    >
      {children}
    </div>
  );
}

function Group({ title, count, children }: { title: string; count: number; children: ReactNode }) {
  const [open, setOpen] = useState(true);
  if (count === 0) return null;
  return (
    <div role="rowgroup">
      <button
        type="button"
        onClick={() => setOpen(!open)}
        aria-expanded={open}
        className="flex h-8 w-full items-center gap-1.5 border-b border-border bg-surface-muted/50 px-2 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground"
      >
        {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        {title}
        <span className="rounded-full bg-surface px-1.5 font-normal normal-case">{count}</span>
      </button>
      {open && children}
    </div>
  );
}

function ConnectionLabel({ profile }: { profile: ConnectionProfile | undefined }) {
  if (!profile) return null;
  return (
    <span className="flex min-w-0 items-center gap-1.5 text-xs text-muted-foreground">
      <span
        className="size-2 shrink-0 rounded-full"
        style={{ backgroundColor: profile.color ?? 'var(--color-border)' }}
      />
      <span className="truncate">{profile.name}</span>
    </span>
  );
}

function Empty({ children }: { children: ReactNode }) {
  return <p className="px-4 py-10 text-center text-sm text-muted-foreground">{children}</p>;
}

const matches = (query: string, ...texts: (string | null | undefined)[]) => {
  const hay = texts.filter(Boolean).join(' ').toLowerCase();
  return query
    .trim()
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean)
    .every((w) => hay.includes(w));
};

// ---- sections --------------------------------------------------------------------------------

function WorkSection({
  items,
  profiles,
}: {
  items: WorkItem[];
  profiles: Map<Uuid, ConnectionProfile>;
}) {
  const { t } = useTranslation('launchpad');
  const title = useTitle();
  const workspaces = useWorkspaces((s) => s.items);
  const scopeName = (scope: string) => workspaces.find((w) => w.id === scope)?.name;
  const current = useTabsStore((s) => s.scope);
  if (items.length === 0) return <Empty>{t('work.empty')}</Empty>;
  const end = async (item: WorkItem, how: 'commit' | 'rollback') => {
    if (!item.tabStateId || !item.sessionId) return;
    if (await endTransaction(item.tabStateId, item.sessionId, how))
      toast.success(t(how === 'commit' ? 'work.committed' : 'work.rolledBack'));
  };
  return (
    <>
      {WORK_ORDER.map((kind) => {
        const list = items.filter((i) => i.kind === kind);
        return (
          <Group key={kind} title={t(`work.kind.${kind}`)} count={list.length}>
            {list.map((item) => {
              const open = () => goToTab(item.tabId, item.scope);
              const other = item.scope !== current;
              return (
                <Row key={item.key} onOpen={open} testId="launchpad-work">
                  <span className="flex min-w-0 items-center gap-2">
                    {WORK_ICON[item.kind]}
                    <span className="min-w-0">
                      <span className="block truncate font-medium">{title(item.title)}</span>
                      <ConnectionLabel
                        profile={item.connectionId ? profiles.get(item.connectionId) : undefined}
                      />
                    </span>
                    {other && (
                      <span className="shrink-0 rounded bg-surface-muted px-1.5 text-[11px] text-muted-foreground">
                        {scopeName(item.scope) ?? t('noWorkspace')}
                      </span>
                    )}
                  </span>
                  <span className="truncate text-xs text-muted-foreground">
                    {item.kind === 'transaction' && t(`work.tx.${item.state ?? 'active'}`)}
                    {item.kind === 'running' &&
                      item.since &&
                      t('work.runningFor', { time: ago(item.since) })}
                    {item.kind === 'pendingEdits' && t('work.changes', { count: item.count ?? 0 })}
                    {item.kind === 'unsavedFile' && item.state}
                  </span>
                  <span className="flex items-center justify-end gap-1">
                    {item.kind === 'transaction' && (
                      <>
                        <Button
                          size="sm"
                          disabled={!item.sessionId}
                          onClick={() => void end(item, 'commit')}
                        >
                          {t('work.commit')}
                        </Button>
                        <Button
                          size="sm"
                          disabled={!item.sessionId}
                          onClick={() => void end(item, 'rollback')}
                        >
                          {t('work.rollback')}
                        </Button>
                      </>
                    )}
                    {item.kind === 'running' && item.tabStateId && (
                      <Button size="sm" onClick={() => void cancelRun(item.tabStateId!)}>
                        {t('work.cancel')}
                      </Button>
                    )}
                    <Button size="sm" variant="primary" onClick={open}>
                      {t('goTo')}
                    </Button>
                  </span>
                </Row>
              );
            })}
          </Group>
        );
      })}
    </>
  );
}

const STATUS_RANK: Record<ConnectionStatus, number> = {
  failed: 0,
  gaveUp: 0,
  lost: 0,
  reconnecting: 1,
  opening: 1,
  connected: 2,
  idle: 3,
};

function ConnectionsSection({ profiles }: { profiles: ConnectionProfile[] }) {
  const { t } = useTranslation('launchpad');
  const { t: tw } = useTranslation('workspaces');
  const status = useConnectionStatus();
  const sessions = useSessionsStore((s) => s.sessionsByConnection);
  const errors = useConnectErrors((s) => s.byConnection);
  if (profiles.length === 0) return <Empty>{t('connections.empty')}</Empty>;
  const sorted = [...profiles].sort(
    (a, b) => STATUS_RANK[status(a.id)] - STATUS_RANK[status(b.id)],
  );
  const groups: { key: string; list: ConnectionProfile[] }[] = [
    { key: 'problems', list: sorted.filter((p) => PROBLEM_STATUSES.has(status(p.id))) },
    {
      key: 'open',
      list: sorted.filter((p) => STATUS_RANK[status(p.id)] === 1 || status(p.id) === 'connected'),
    },
    { key: 'idle', list: sorted.filter((p) => status(p.id) === 'idle') },
  ];
  return (
    <>
      {groups.map((g) => (
        <Group key={g.key} title={t(`connections.group.${g.key}`)} count={g.list.length}>
          {g.list.map((p) => {
            const st = status(p.id);
            const session = sessions[p.id];
            const navigate = () => {
              useNavigation.getState().navigate(p.id);
              useLaunchpad.getState().setOpen(false);
            };
            return (
              <Row key={p.id} onOpen={navigate} testId="launchpad-connection">
                <span className="flex min-w-0 items-center gap-2">
                  <span className="w-[24px] shrink-0 rounded bg-surface-muted text-center text-[9px] font-semibold leading-4 text-muted-foreground">
                    {kindBadgeFor(p.kind)}
                  </span>
                  <span className="min-w-0">
                    <span className="block truncate font-medium">{p.name}</span>
                    <span className="block truncate text-[11px] text-muted-foreground">
                      {[p.host && `${p.host}:${p.port}`, p.database].filter(Boolean).join(' / ')}
                    </span>
                  </span>
                </span>
                <span
                  className={cn(
                    'truncate text-xs',
                    PROBLEM_STATUSES.has(st) ? 'text-danger' : 'text-muted-foreground',
                  )}
                  title={errors[p.id]?.message}
                >
                  {tw(`status.${st}`)}
                  {errors[p.id] && !session ? ` · ${errors[p.id]!.message.split('\n')[0]}` : ''}
                </span>
                <span className="flex items-center justify-end gap-1">
                  {session ? (
                    <>
                      <Button
                        size="sm"
                        disabled={session.descriptor.capabilities.sql === false}
                        onClick={() => {
                          openEditor(session);
                          useLaunchpad.getState().setOpen(false);
                        }}
                      >
                        {t('connections.newEditor')}
                      </Button>
                      <Button size="sm" onClick={() => void disconnectMany([p.id])}>
                        {t('connections.disconnect')}
                      </Button>
                    </>
                  ) : (
                    <Button size="sm" variant="primary" onClick={navigate}>
                      {PROBLEM_STATUSES.has(st) ? t('connections.retry') : t('connections.connect')}
                    </Button>
                  )}
                </span>
              </Row>
            );
          })}
        </Group>
      ))}
    </>
  );
}

function ObjectsSection({
  favorites,
  recent,
  profiles,
}: {
  favorites: SavedObject[];
  recent: SavedObject[];
  profiles: Map<Uuid, ConnectionProfile>;
}) {
  const { t } = useTranslation('launchpad');
  const toggle = useNavigation((s) => s.toggle);
  const pinned = new Set(favorites.map((f) => f.key));
  if (favorites.length + recent.length === 0) return <Empty>{t('objects.empty')}</Empty>;
  const row = (o: SavedObject) => {
    const open = () => {
      useNavigation.getState().navigate(o.connectionId, o);
      useLaunchpad.getState().setOpen(false);
    };
    const isPinned = pinned.has(o.key);
    return (
      <Row key={o.key} onOpen={open} testId="launchpad-object">
        <span className="flex min-w-0 items-center gap-2">
          <span className="w-10 shrink-0 text-[11px] text-muted-foreground uppercase">
            {t(`objects.kind.${o.kind}`)}
          </span>
          <span className="min-w-0">
            <span
              className="block truncate font-medium"
              title={o.path.segments.map((s) => s.name).join('.')}
            >
              {o.name}
            </span>
            <ConnectionLabel profile={profiles.get(o.connectionId)} />
          </span>
        </span>
        <span className="truncate text-xs text-muted-foreground">
          {o.path.segments
            .slice(0, -1)
            .filter((s) => s.kind !== 'folder')
            .map((s) => s.name)
            .join(' › ')}
        </span>
        <span className="flex items-center justify-end gap-1">
          <Button
            size="icon"
            variant="ghost"
            aria-label={isPinned ? t('objects.unpin') : t('objects.pin')}
            title={isPinned ? t('objects.unpin') : t('objects.pin')}
            onClick={() => toggle(o)}
          >
            {isPinned ? <StarOff size={14} /> : <Star size={14} />}
          </Button>
          <Button size="sm" variant="primary" onClick={open}>
            {t('objects.open')}
          </Button>
        </span>
      </Row>
    );
  };
  return (
    <>
      <Group title={t('objects.pinned')} count={favorites.length}>
        {favorites.map(row)}
      </Group>
      <Group title={t('objects.recent')} count={recent.length}>
        {recent.map(row)}
      </Group>
    </>
  );
}

function JobsSection({
  jobs,
  profiles,
}: {
  jobs: JobInfo[];
  profiles: Map<Uuid, ConnectionProfile>;
}) {
  const { t } = useTranslation('launchpad');
  const { t: tj } = useTranslation('jobs');
  if (jobs.length === 0) return <Empty>{t('jobs.empty')}</Empty>;
  const running = jobs.filter(isActive);
  const failed = jobs.filter((j) => j.status === 'failed');
  const done = jobs.filter((j) => !isActive(j) && j.status !== 'failed');
  const row = (job: JobInfo) => {
    const fraction = jobFraction(job);
    const openPanel = () => {
      useJobsStore.getState().setPanelOpen(true);
      useLaunchpad.getState().setOpen(false);
    };
    return (
      <Row key={job.id} onOpen={openPanel} testId="launchpad-job">
        <span className="min-w-0">
          <span className="block truncate font-medium">{job.title}</span>
          <ConnectionLabel
            profile={job.connectionId ? profiles.get(job.connectionId) : undefined}
          />
        </span>
        <span className="min-w-0 text-xs text-muted-foreground">
          <span className={cn(job.status === 'failed' && 'text-danger')}>
            {tj(`status.${job.status}`, job.status)}
            {isActive(job) && fraction !== null && ` · ${Math.round(fraction * 100)}%`}
            {job.error && ` · ${job.error.message.split('\n')[0]}`}
          </span>
          {isActive(job) && fraction !== null && (
            <span className="mt-1 block h-1 overflow-hidden rounded bg-surface-muted">
              <span className="block h-full bg-accent" style={{ width: `${fraction * 100}%` }} />
            </span>
          )}
        </span>
        <span className="flex items-center justify-end gap-1">
          {isActive(job) && job.cancellable && (
            <Button
              size="sm"
              onClick={() =>
                void ipc.jobCancel(job.id).catch((e) => toast.error(toIpcError(e).message))
              }
            >
              {t('jobs.cancel')}
            </Button>
          )}
          <Button size="sm" variant="primary" onClick={openPanel}>
            {t('jobs.details')}
          </Button>
        </span>
      </Row>
    );
  };
  return (
    <>
      <Group title={t('jobs.running')} count={running.length}>
        {running.map(row)}
      </Group>
      <Group title={t('jobs.failed')} count={failed.length}>
        {failed.map(row)}
      </Group>
      <Group title={t('jobs.done')} count={done.length}>
        {done.slice(0, 30).map(row)}
      </Group>
    </>
  );
}

// ---- the page --------------------------------------------------------------------------------

/** The Launchpad page; it covers the tab content while open. */
export function LaunchpadView() {
  const { t } = useTranslation('launchpad');
  const section = useLaunchpad((s) => s.section);
  const setSection = useLaunchpad((s) => s.setSection);
  const scope = useLaunchpad((s) => s.scope);
  const setScope = useLaunchpad((s) => s.setScope);
  const ws = useWorkspaces((s) => activeWorkspace(s));
  const { data: snapshot } = useConnectionsSnapshot();
  const work = useWork();
  const favorites = useNavigation((s) => s.favorites);
  const recent = useNavigation((s) => s.recent);
  const jobsById = useJobsStore((s) => s.jobs);
  const title = useTitle();
  const [query, setQuery] = useState('');
  const searchRef = useRef<HTMLInputElement>(null);
  const rootRef = useRef<HTMLElement>(null);

  const profiles = useMemo(
    () => new Map((snapshot?.profiles ?? []).map((p) => [p.id, p])),
    [snapshot],
  );
  const all = scope === 'all' || !ws;
  const opts = { all, workspace: ws?.id ?? '', members: ws ? new Set(ws.connections) : null };
  const nameOf = (id: Uuid | null | undefined) => (id ? profiles.get(id)?.name : '');

  const workItems = inScope(work, opts, 'tab').filter((w) =>
    matches(query, title(w.title), nameOf(w.connectionId), w.state),
  );
  const connections = (snapshot?.profiles ?? [])
    .filter((p) => all || ws!.connections.includes(p.id))
    .filter((p) => matches(query, p.name, p.host, p.database, p.kind));
  const known = (o: SavedObject) => profiles.has(o.connectionId);
  const objFilter = (o: SavedObject) =>
    known(o) &&
    matches(query, o.name, nameOf(o.connectionId), o.path.segments.map((s) => s.name).join(' '));
  const favs = inScope(favorites, opts, 'connection').filter(objFilter);
  const favKeys = new Set(favs.map((f) => f.key));
  const recents = inScope(recent, opts, 'connection').filter(
    (o) => objFilter(o) && !favKeys.has(o.key),
  );
  const jobs = inScope(sortedJobs(jobsById), opts, 'connection').filter((j) =>
    matches(query, j.title, nameOf(j.connectionId)),
  );
  const status = useConnectionStatus();
  const counts: Record<LaunchpadSection, number> = {
    work: workItems.length,
    connections: connections.filter((p) => status(p.id) !== 'idle').length,
    objects: favs.length + recents.length,
    jobs: jobs.filter((j) => isActive(j) || j.status === 'failed').length,
  };

  // `/` focuses the search; J/K (and the arrows) move between rows.
  useEffect(() => {
    const onKey = (e: globalThis.KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      const typing =
        !!target &&
        (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.isContentEditable);
      if (typing || e.ctrlKey || e.metaKey || e.altKey) return;
      if (document.querySelector('[role="menu"], [role="dialog"]')) return;
      if (e.key === '/') {
        e.preventDefault();
        searchRef.current?.focus();
        return;
      }
      const down = e.key === 'j' || e.key === 'ArrowDown';
      const up = e.key === 'k' || e.key === 'ArrowUp';
      if (!down && !up) return;
      const rows = Array.from(
        rootRef.current?.querySelectorAll<HTMLElement>('[data-lp-row]') ?? [],
      );
      if (rows.length === 0) return;
      e.preventDefault();
      const at = rows.indexOf(document.activeElement as HTMLElement);
      const next = rows[at < 0 ? 0 : Math.min(rows.length - 1, Math.max(0, at + (down ? 1 : -1)))];
      next?.focus();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  const onSearchKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Escape') {
      if (query) setQuery('');
      else e.currentTarget.blur();
    } else if (e.key === 'ArrowDown') {
      e.preventDefault();
      rootRef.current?.querySelector<HTMLElement>('[data-lp-row]')?.focus();
    }
  };

  return (
    <section
      ref={rootRef}
      className="h-full overflow-y-auto"
      aria-labelledby="launchpad-title"
      data-testid="launchpad"
    >
      <div className="w-full px-8 py-6">
        <header className="flex flex-wrap items-start gap-3">
          <Rocket size={22} className="mt-0.5 text-accent" />
          <div className="mr-auto">
            <h1 id="launchpad-title" className="text-xl font-semibold">
              {t('title')}
            </h1>
            <p className="text-xs text-muted-foreground">{t('subtitle')}</p>
          </div>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button size="sm" disabled={!ws} data-testid="launchpad-scope">
                {all ? t('scope.all') : t('scope.workspace', { name: ws!.name })}
                <ChevronDown size={12} />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuRadioGroup
                value={scope}
                onValueChange={(v) => setScope(v as 'workspace' | 'all')}
              >
                <DropdownMenuRadioItem value="workspace">
                  {t('scope.workspace', { name: ws?.name ?? '' })}
                </DropdownMenuRadioItem>
                <DropdownMenuRadioItem value="all">{t('scope.all')}</DropdownMenuRadioItem>
              </DropdownMenuRadioGroup>
            </DropdownMenuContent>
          </DropdownMenu>
        </header>

        <div
          role="tablist"
          aria-label={t('sections')}
          className="mt-5 flex flex-wrap gap-1 border-b border-border"
        >
          {SECTIONS.map((s) => (
            <button
              key={s}
              type="button"
              role="tab"
              aria-selected={section === s}
              onClick={() => setSection(s)}
              className={cn(
                '-mb-px flex h-9 items-center gap-2 border-b-2 px-3 text-[13px]',
                section === s
                  ? 'border-accent font-medium text-foreground'
                  : 'border-transparent text-muted-foreground hover:text-foreground',
              )}
            >
              {t(`section.${s}`)}
              <span className="min-w-8 rounded-full bg-surface-muted px-1.5 text-center text-[11px] tabular-nums">
                {counts[s]}
              </span>
            </button>
          ))}
        </div>

        <div className="mt-3 flex items-center gap-2">
          <Input
            ref={searchRef}
            type="search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={onSearchKey}
            placeholder={t('search')}
            aria-label={t('search')}
            className="h-7 w-72 text-xs"
          />
        </div>

        <div
          role="table"
          aria-label={t(`section.${section}`)}
          className="mt-3 rounded-md border border-border"
        >
          {section === 'work' && <WorkSection items={workItems} profiles={profiles} />}
          {section === 'connections' && <ConnectionsSection profiles={connections} />}
          {section === 'objects' && (
            <ObjectsSection favorites={favs} recent={recents} profiles={profiles} />
          )}
          {section === 'jobs' && <JobsSection jobs={jobs} profiles={profiles} />}
        </div>
        <p className="mt-3 text-[11px] text-muted-foreground">{t('keysHint')}</p>
      </div>
    </section>
  );
}
