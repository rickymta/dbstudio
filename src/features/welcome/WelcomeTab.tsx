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
            : t('status.planned', { phase: driver.status.phase })}
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
    </section>
  );
}
