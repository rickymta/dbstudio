import { Button } from '@/components/ui/Button';
import { useConnectionsSnapshot } from '@/features/connections/queries';
import { useNavigation } from '@/features/workspace/navigation';
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

export function WelcomeTab() {
  const connections = useConnectionsSnapshot();
  const recent = useNavigation((s) => s.recent);
  const navigate = useNavigation((s) => s.navigate);
  const tabs = useTabsStore((s) => s.tabs);
  const { t } = useTranslation();
  const { data, isPending, isError } = useQuery({
    queryKey: ['drivers'],
    queryFn: ipc.listDrivers,
  });

  return (
    <section className="h-full overflow-y-auto p-6" aria-labelledby="welcome-title">
      <h1 id="welcome-title" className="text-xl font-semibold">
        {t('welcome.title')}
      </h1>
      <p className="mt-1 text-muted-foreground">{t('welcome.subtitle')}</p>
      <div className="mt-5 flex flex-wrap gap-2">
        <Button variant="primary" onClick={() => useNavigation.getState().newConnection()}>
          {t('workspace.startConnection')}
        </Button>
      </div>
      <div className="mt-6 grid grid-cols-[repeat(auto-fit,minmax(240px,1fr))] gap-5">
        <section>
          <h2 className="mb-2 font-medium">{t('workspace.continue')}</h2>
          {tabs
            .filter((tab) => tab.kind === 'editor')
            .map((tab) => (
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
          {recent
            .filter((object) =>
              connections.data?.profiles.some((p) => p.id === object.connectionId),
            )
            .slice(0, 6)
            .map((object) => (
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
          {!recent.length && !tabs.some((tab) => tab.kind === 'editor') && (
            <p className="text-muted-foreground">{t('workspace.noRecent')}</p>
          )}
        </section>
        <section>
          <h2 className="mb-2 font-medium">{t('workspace.savedConnections')}</h2>
          {connections.data?.profiles.slice(0, 8).map((profile) => (
            <button
              key={profile.id}
              className="block min-h-9 w-full truncate rounded px-2 text-left hover:bg-surface-muted"
              onClick={() => navigate(profile.id)}
            >
              {profile.name}
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
        {isError && <p role="alert">{t('welcome.loadError')}</p>}
        {isPending && <p className="text-muted-foreground">…</p>}
        {data && (
          <ul className="grid grid-cols-[repeat(auto-fill,minmax(240px,1fr))] gap-3">
            {data.map((d) => (
              <DriverCard key={d.kind} driver={d} />
            ))}
          </ul>
        )}
      </details>
    </section>
  );
}
