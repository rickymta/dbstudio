import { Clock, KeyRound, Pencil, RefreshCw, Trash2 } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/Badge';
import { Button } from '@/components/ui/Button';
import { ReadOnlyBadge } from '@/features/safety/ReadOnlyBadge';
import type { KeyValuePage } from '@/lib/types';
import { displayKeyName, formatDuration } from './keyDetail';

interface KeyHeaderProps {
  name: string;
  page: KeyValuePage | null;
  fetchedAt: number;
  readOnly: boolean;
  busy: boolean;
  onRefresh: () => void;
  onTtl: () => void;
  onRename: () => void;
  onDelete: () => void;
}

/** Remaining TTL in ms, counting down once per second from the moment of the read. */
function useRemainingMs(ttlMs: number | null | undefined, fetchedAt: number): number | null {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (ttlMs === null || ttlMs === undefined) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [ttlMs, fetchedAt]);
  if (ttlMs === null || ttlMs === undefined) return null;
  return Math.max(0, ttlMs - Math.max(0, now - fetchedAt));
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / 1024 / 1024).toFixed(1)} MiB`;
}

/** Key name, type, TTL countdown, size info and the key-level actions. */
export function KeyHeader({
  name,
  page,
  fetchedAt,
  readOnly,
  busy,
  onRefresh,
  onTtl,
  onRename,
  onDelete,
}: KeyHeaderProps) {
  const { t } = useTranslation('keyvalue');
  const remaining = useRemainingMs(page?.ttlMs, fetchedAt);
  return (
    <header className="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-border px-3 py-2">
      <KeyRound size={14} className="shrink-0 text-muted-foreground" aria-hidden />
      <h2 className="min-w-0 truncate font-mono text-[13px] font-semibold" title={name}>
        {displayKeyName(name)}
      </h2>
      {page && <Badge data-testid="key-type">{page.keyType}</Badge>}
      <ReadOnlyBadge readOnly={readOnly} />
      {page && (
        <dl className="flex flex-wrap items-center gap-x-3 text-xs text-muted-foreground">
          <div className="flex items-center gap-1" data-testid="key-ttl">
            <Clock size={12} aria-hidden />
            <dt className="sr-only">{t('header.ttl')}</dt>
            <dd>
              {remaining === null
                ? t('header.persistent')
                : remaining <= 0
                  ? t('header.expired')
                  : formatDuration(remaining / 1000)}
            </dd>
          </div>
          {page.length !== null && (
            <div className="flex gap-1">
              <dt>{t('header.length')}</dt>
              <dd data-testid="key-length">{page.length}</dd>
            </div>
          )}
          {page.memoryBytes !== null && (
            <div className="flex gap-1">
              <dt>{t('header.memory')}</dt>
              <dd>{formatBytes(page.memoryBytes)}</dd>
            </div>
          )}
          {page.encoding && (
            <div className="flex gap-1">
              <dt>{t('header.encoding')}</dt>
              <dd>{page.encoding}</dd>
            </div>
          )}
        </dl>
      )}
      <div className="ml-auto flex items-center gap-1">
        <Button size="sm" onClick={onRefresh} disabled={busy} aria-label={t('actions.refresh')}>
          <RefreshCw size={12} /> {t('actions.refresh')}
        </Button>
        <Button size="sm" onClick={onTtl} disabled={busy || readOnly || !page}>
          <Clock size={12} /> {t('actions.ttl')}
        </Button>
        <Button size="sm" onClick={onRename} disabled={busy || readOnly || !page}>
          <Pencil size={12} /> {t('actions.rename')}
        </Button>
        <Button size="sm" variant="danger" onClick={onDelete} disabled={busy || readOnly || !page}>
          <Trash2 size={12} /> {t('actions.deleteKey')}
        </Button>
      </div>
    </header>
  );
}
