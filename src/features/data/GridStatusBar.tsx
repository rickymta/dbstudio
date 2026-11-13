import { Octagon, RefreshCw } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { useSessionsStore } from '@/stores/sessions';
import { Button } from '@/components/ui/Button';
import { IconButton } from '@/components/ui/IconButton';
import { Spinner } from '@/components/ui/Spinner';
import type { GridModel } from './gridModel';
import { useGridVersion } from './useGridVersion';

interface GridStatusBarProps {
  model: GridModel;
}

/** Loaded/total counts, last fetch time, Stop/Refresh, and load notices. */
export function GridStatusBar({ model }: GridStatusBarProps) {
  const { t, i18n } = useTranslation('data');
  useGridVersion(model);
  const nf = new Intl.NumberFormat(i18n.language);
  const loading = model.status === 'loading';
  const loaded = model.rows.length;

  // Stop only makes sense when the driver can cancel a query (NFR-EXT-02).
  const canCancel = useSessionsStore((s) =>
    Object.values(s.sessionsByConnection).some(
      (x) => x.sessionId === model.sessionId && x.descriptor.capabilities.cancelQuery,
    ),
  );

  const notice = model.capped
    ? model.capReason === 'cells'
      ? t('grid.cappedCells', {
          rows: nf.format(loaded),
          cells: nf.format(model.maxCells),
          columns: model.columns.length,
        })
      : t('grid.cappedRows', { max: nf.format(model.maxRows) })
    : model.status === 'stopped'
      ? t('grid.stopped')
      : null;
  const inlineError = model.status === 'error' && loaded > 0 ? model.error : null;

  return (
    <div className="shrink-0 border-t border-border bg-surface-muted text-xs">
      {(notice || inlineError) && (
        <div
          role={inlineError ? 'alert' : 'status'}
          className={
            inlineError
              ? 'flex items-center gap-2 bg-danger-soft px-3 py-1 text-danger'
              : 'bg-warning-soft px-3 py-1 text-warning'
          }
        >
          <span className="min-w-0 flex-1 break-words">
            {inlineError ? `${t('grid.errorTitle')}: ${inlineError.message}` : notice}
          </span>
          {inlineError && (
            <Button size="sm" onClick={() => model.retry()}>
              {t('grid.retry')}
            </Button>
          )}
        </div>
      )}
      <div className="flex h-7 items-center gap-3 px-3 text-muted-foreground">
        <span className="tabular-nums text-foreground">
          {loaded === 1 ? t('status.loadedOne') : t('status.loaded', { count: nf.format(loaded) })}
          {model.total !== null && ` ${t('status.total', { total: nf.format(model.total) })}`}
        </span>
        {model.total === null && !model.counting && (
          <Button
            size="sm"
            variant="ghost"
            title={t('status.countTitle')}
            onClick={() => model.count()}
          >
            {t('status.count')}
          </Button>
        )}
        {model.counting && (
          <span className="flex items-center gap-1.5">
            <Spinner />
            <span>{t('status.counting')}</span>
            {canCancel && (
              <Button size="sm" variant="outline" onClick={() => model.cancelCount()}>
                {t('status.cancelCount')}
              </Button>
            )}
          </span>
        )}
        {model.countError && (
          <span
            className="max-w-48 truncate text-danger"
            title={t('status.countError', { message: model.countError.message })}
          >
            {t('status.countFailed')}
          </span>
        )}
        {model.lastFetchMs !== null && (
          <span className="tabular-nums">{t('status.fetchTime', { ms: model.lastFetchMs })}</span>
        )}
        <span className="flex-1" />
        {loading && (
          <>
            <Spinner />
            <span>{t('status.loadingState')}</span>
            {canCancel && (
              <Button size="sm" variant="outline" onClick={() => model.stop()}>
                <Octagon size={11} /> {t('status.stop')}
              </Button>
            )}
          </>
        )}
        <IconButton
          label={t('status.refresh')}
          icon={<RefreshCw size={13} />}
          onClick={() => model.reload()}
        />
      </div>
    </div>
  );
}
