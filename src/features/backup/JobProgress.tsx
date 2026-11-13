import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/Badge';
import type { JobInfo } from '@/lib/types';
import { formatBytes } from './format';

const STATUS_CLASS: Record<string, string> = {
  running: 'bg-accent/15 text-accent',
  queued: 'bg-surface-muted text-muted-foreground',
  succeeded: 'bg-success-soft text-success',
  failed: 'bg-danger-soft text-danger',
  cancelled: 'bg-surface-muted text-muted-foreground',
  interrupted: 'bg-warning-soft text-warning',
};

/** Colored status pill for a job or run status. */
export function StatusBadge({ status }: { status: string }) {
  const { t } = useTranslation('backup');
  return (
    <Badge className={STATUS_CLASS[status] ?? ''} data-status={status}>
      {t(`status.${status}`)}
    </Badge>
  );
}

/** Progress of one background job (phase, counters, bar) or its final outcome. */
export function JobProgressView({ job }: { job: JobInfo }) {
  const { t } = useTranslation('backup');
  const p = job.progress;
  const total = p.objectsTotal ?? 0;
  const pct = total > 0 ? Math.min(100, Math.round((p.objectsDone / total) * 100)) : null;
  const active = job.status === 'running' || job.status === 'queued';
  return (
    <div className="flex flex-col gap-1" data-testid="job-progress" data-status={job.status}>
      <div className="flex items-center gap-2 text-xs">
        <StatusBadge status={job.status} />
        <span className="font-medium">{job.title}</span>
        {active && (
          <span className="text-muted-foreground">
            {t(`phase.${p.phase}`, { defaultValue: p.phase })}
            {p.object ? ` - ${p.object}` : ''}
          </span>
        )}
      </div>
      {active && (
        <>
          <div
            className="h-1.5 overflow-hidden rounded bg-surface-muted"
            role="progressbar"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={pct ?? undefined}
          >
            <div
              className={pct === null ? 'h-full w-1/3 animate-pulse bg-accent' : 'h-full bg-accent'}
              style={pct === null ? undefined : { width: `${pct}%` }}
            />
          </div>
          <p className="text-[11px] text-muted-foreground">
            {t('progress.counters', {
              objects: p.objectsDone,
              rows: p.rowsDone,
              bytes: formatBytes(p.bytesDone),
            })}
          </p>
        </>
      )}
      {job.error && (
        <p role="alert" className="text-xs text-danger">
          {job.error.message}
        </p>
      )}
    </div>
  );
}
