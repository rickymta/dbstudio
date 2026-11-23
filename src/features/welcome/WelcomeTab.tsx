import { Database, Plus, Rocket } from 'lucide-react';
import { useLaunchpad } from '@/features/launchpad/store';
import { Button } from '@/components/ui/Button';
import { useConnectionsSnapshot } from '@/features/connections/queries';
import { useNavigation } from '@/features/workspace/navigation';
import { useActiveWorkspace } from '@/features/workspaces/store';
import { WorkspaceView } from '@/features/workspaces/WorkspaceView';
import { useTabsStore } from '@/stores/tabs';
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/Badge';
import { ipc } from '@/lib/ipc';
import type { Capabilities, DriverDescriptor } from '@/lib/types';

/** Capabilities highlighted on each card, in display order. */
const HIGHLIGHTED: Array<keyof Capabilities> = [
  'sql',
  'transactions',
  'explain',
  'foreignKeys',
  'dataEditing',
];

function DriverCard({ driver }: { driver: DriverDescriptor }) {
  const { t } = useTranslation();
  const available = driver.status.state === 'available';
  const caps = HIGHLIGHTED.filter((c) => driver.capabilities[c]);

  return (
    <li
      className="rounded-lg border border-border bg-surface p-3"
      data-testid={`driver-${driver.kind}`}
    >
      <div className="flex items-start justify-between gap-2">
        <h3 className="text-sm font-semibold">{driver.displayName}</h3>
        <Badge tone={available ? 'success' : 'warning'}>
          {driver.status.state === 'available'
            ? t('status.available')
            : driver.status.state === 'planned'
              ? t('status.planned', { phase: driver.status.phase })
              : t('status.unavailable')}
        </Badge>
      </div>
      <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-xs">
        <dt className="text-muted-foreground">{t('welcome.family')}</dt>
        <dd>{t(`family.${driver.family}`)}</dd>
        <dt className="text-muted-foreground">{t('welcome.port')}</dt>
        <dd className="font-mono">{driver.defaultPort}</dd>
      </dl>
      <div className="mt-2 flex flex-wrap gap-1">
        {caps.map((c) => (
          <Badge key={c}>{t(`capability.${c}`)}</Badge>
        ))}
      </div>
    </li>
  );
}

/** First tab: the active workspace's overview, else the welcome page. */
export function WelcomeTab() {
  const workspace = useActiveWorkspace();
  return workspace ? <WorkspaceView ws={workspace} /> : <WelcomePage />;
}

function WelcomePage() {
  const connections = useConnectionsSnapshot();
  const recent = useNavigation((s) => s.recent);
  const navigate = useNavigation((s) => s.navigate);
  const tabs = useTabsStore((s) => s.tabs);
  const { t } = useTranslation();
  const visibleRecent = recent
    .filter((object) => connections.data?.profiles.some((p) => p.id === object.connectionId))
    .slice(0, 6);
  const editors = tabs.filter((tab) => tab.kind === 'editor');
  const { data, isPending, isError, refetch } = useQuery({
    queryKey: ['drivers'],
    queryFn: ipc.listDrivers,
  });

  return (
    <section className="h-full overflow-y-auto p-4 sm:p-6" aria-labelledby="welcome-title">
      <div className="mx-auto max-w-5xl">
        <header className="rounded-xl border border-border bg-surface p-5">
          <h1 id="welcome-title" className="text-xl font-semibold">
            {t('welcome.title')}
          </h1>
          <p className="mt-1 text-muted-foreground">{t('welcome.subtitle')}</p>
          <div className="mt-5 flex flex-wrap gap-2">
            <Button variant="primary" onClick={() => useNavigation.getState().newConnection()}>
              <Plus size={15} aria-hidden /> {t('workspace.startConnection')}
            </Button>
            <Button onClick={() => useLaunchpad.getState().setOpen(true)}>
              <Rocket size={15} aria-hidden /> {t('welcome.launchpad')}
            </Button>
          </div>
          <p className="mt-3 text-xs text-muted-foreground">{t('welcome.sessionHint')}</p>
        </header>
        <div className="mt-6 grid grid-cols-[repeat(auto-fit,minmax(min(100%,260px),1fr))] gap-5">
          <section className="min-w-0 rounded-xl border border-border bg-surface p-4">
            <h2 className="mb-2 font-medium">{t('workspace.continue')}</h2>
            {editors.map((tab) => (
              <button
                key={tab.id}
                className="block min-h-9 w-full truncate rounded px-2 text-left hover:bg-surface-muted"
                onClick={() => useTabsStore.getState().activate(tab.id)}
              >
                {typeof tab.title === 'string'
                  ? tab.title
                  : t(tab.title.key, { ns: tab.title.ns ?? 'common' })}
              </button>
            ))}
            {visibleRecent.map((object) => (
              <button
                key={object.key}
                className="flex min-h-9 w-full items-center gap-2 rounded px-2 text-left hover:bg-surface-muted"
                title={object.path.segments.map((s) => s.name).join('.')}
                onClick={() => navigate(object.connectionId, object)}
              >
                <span className="min-w-0 flex-1 truncate">{object.name}</span>
                <span className="max-w-[45%] truncate text-xs text-muted-foreground">
                  {connections.data?.profiles.find((p) => p.id === object.connectionId)?.name}
                </span>
              </button>
            ))}
            {!visibleRecent.length && !editors.length && (
              <p className="text-muted-foreground">{t('workspace.noRecent')}</p>
            )}
          </section>
          <section className="min-w-0 rounded-xl border border-border bg-surface p-4">
            <h2 className="mb-2 font-medium">{t('workspace.savedConnections')}</h2>
            {connections.isPending && (
              <p role="status" className="text-muted-foreground">
                {t('spinner.loading')}
              </p>
            )}
            {connections.isError && (
              <div role="alert">
                <p>{t('welcome.connectionsError')}</p>
                <Button size="sm" onClick={() => void connections.refetch()}>
                  {t('actions.retry')}
                </Button>
              </div>
            )}
            {connections.isSuccess && !connections.data.profiles.length && (
              <p className="text-muted-foreground">{t('welcome.noConnections')}</p>
            )}
            {connections.data?.profiles.slice(0, 8).map((profile) => (
              <button
                key={profile.id}
                className="flex min-h-12 w-full items-center gap-3 rounded px-2 text-left hover:bg-surface-muted"
                onClick={() => navigate(profile.id)}
              >
                <Database size={16} className="shrink-0 text-muted-foreground" aria-hidden />
                <span className="min-w-0">
                  <span className="block truncate text-sm">{profile.name}</span>
                  <span className="block truncate text-xs text-muted-foreground">
                    {profile.kind} · {profile.host}
                  </span>
                </span>
              </button>
            ))}
          </section>
        </div>
        <details className="mt-8">
          <summary className="cursor-pointer text-muted-foreground">
            {t('workspace.databaseSupport')}
          </summary>
          <h2 className="mt-6 mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
            {t('welcome.supported')}
          </h2>
          {isError && (
            <div role="alert">
              <p>{t('welcome.loadError')}</p>
              <Button size="sm" onClick={() => void refetch()}>
                {t('actions.retry')}
              </Button>
            </div>
          )}
          {isPending && (
            <p role="status" className="text-muted-foreground">
              {t('spinner.loading')}
            </p>
          )}
          {data && (
            <ul className="grid grid-cols-[repeat(auto-fill,minmax(min(100%,240px),1fr))] gap-3">
              {data.map((d) => (
                <DriverCard key={d.kind} driver={d} />
              ))}
            </ul>
          )}
        </details>
      </div>
    </section>
  );
}
