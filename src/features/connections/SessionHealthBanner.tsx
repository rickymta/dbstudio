import { CheckCircle2, Unplug, X } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { IconButton } from '@/components/ui/IconButton';
import { Spinner } from '@/components/ui/Spinner';
import { cn } from '@/lib/cn';
import { listenSessionHealth, useSessionHealthStore } from '@/stores/sessionHealth';
import { useTabsStore } from '@/stores/tabs';
import { describeError } from './errors';
import { useConnectionProfile } from './queries';
import { useSessionReconnect } from './useSessionReconnect';

/** How long the "reconnected" notice stays when nothing was lost with the connection. */
const RECONNECTED_MS = 5000;

/**
 * Connection health of the active tab (CONN-12): lost, reconnecting (attempt n), gave up (with a
 * manual Reconnect) and reconnected, with what the outage cost (lost transactions, session state
 * of the reopened leases). Fed by the `session://state` event.
 */
export function SessionHealthBanner() {
  const { t } = useTranslation('connections');
  const connectionId = useTabsStore(
    (s) => s.tabs.find((x) => x.id === s.activeId)?.payload?.connectionId,
  );
  const health = useSessionHealthStore((s) =>
    connectionId ? s.byConnection[connectionId] : undefined,
  );
  const clear = useSessionHealthStore((s) => s.clear);
  const profile = useConnectionProfile(connectionId ?? '');
  const { reconnect, dialog } = useSessionReconnect();
  const [busy, setBusy] = useState(false);
  // Error of the last manual reconnect, tied to the health state it was shown for.
  const [failed, setFailed] = useState<{ at: string; text: string } | null>(null);

  useEffect(() => {
    let off: (() => void) | undefined;
    let cancelled = false;
    void listenSessionHealth().then((unlisten) => {
      if (cancelled) unlisten();
      else off = unlisten;
    });
    return () => {
      cancelled = true;
      off?.();
    };
  }, []);

  const lossless =
    health?.state === 'reconnected' && health.leasesBroken === 0 && health.leasesReopened === 0;
  useEffect(() => {
    if (!lossless || !connectionId) return;
    const timer = setTimeout(() => clear(connectionId), RECONNECTED_MS);
    return () => clearTimeout(timer);
  }, [lossless, connectionId, clear]);

  if (!connectionId || !health) return dialog;

  const name = profile?.name ?? '';
  const at = `${connectionId}:${health.sessionId}:${health.state}`;
  const failure = failed?.at === at ? failed.text : null;
  const retry = async () => {
    setBusy(true);
    setFailed(null);
    try {
      await reconnect(health.sessionId, connectionId);
      clear(connectionId);
    } catch (e) {
      if ((e as { kind?: unknown }).kind !== 'cancelled')
        setFailed({ at, text: describeError(t, e) });
    } finally {
      setBusy(false);
    }
  };

  const ok = health.state === 'reconnected';
  const waiting = health.state === 'lost' || health.state === 'reconnecting';
  return (
    <>
      <div
        role={ok ? 'status' : 'alert'}
        data-testid="session-health"
        data-state={health.state}
        className={cn(
          'flex items-start gap-2 border-b border-border px-3 py-1.5 text-xs',
          ok
            ? 'bg-success-soft text-success'
            : health.state === 'gaveUp'
              ? 'bg-danger-soft text-danger'
              : 'bg-warning-soft text-warning',
        )}
      >
        <span className="mt-0.5 shrink-0">
          {waiting ? (
            <Spinner size={12} />
          ) : ok ? (
            <CheckCircle2 size={13} aria-hidden />
          ) : (
            <Unplug size={13} aria-hidden />
          )}
        </span>
        <div className="flex min-w-0 flex-1 flex-col gap-0.5">
          <span className="font-medium">
            {health.state === 'reconnecting'
              ? t('health.reconnecting', { name, attempt: health.attempt })
              : t(`health.${health.state}`, { name })}
          </span>
          {health.state === 'gaveUp' && health.error && (
            <span className="select-text">{describeError(t, health.error)}</span>
          )}
          {health.leasesBroken > 0 && (
            <span>{t('health.leasesBroken', { count: health.leasesBroken })}</span>
          )}
          {ok && health.leasesReopened > 0 && (
            <span>{t('health.leasesReopened', { count: health.leasesReopened })}</span>
          )}
          {failure && <span className="select-text">{failure}</span>}
        </div>
        {health.state === 'gaveUp' && (
          <Button size="sm" disabled={busy} onClick={() => void retry()}>
            {busy && <Spinner size={12} />}
            {t('health.reconnect')}
          </Button>
        )}
        {!waiting && (
          <IconButton
            label={t('health.dismiss')}
            icon={<X size={13} />}
            onClick={() => clear(connectionId)}
          />
        )}
      </div>
      {dialog}
    </>
  );
}
