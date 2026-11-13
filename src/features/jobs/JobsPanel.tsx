import { Ban, CheckCircle2, CircleAlert, Clock, Loader2, Trash2 } from 'lucide-react';
import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { EmptyState } from '@/components/ui/EmptyState';
import { ipc } from '@/lib/ipc';
import type { JobInfo, JobStatus } from '@/lib/types';
import { isActive, jobFraction, sortedJobs, useJobsStore } from './jobsStore';

const statusIcon: Record<JobStatus, typeof Clock> = {
  queued: Clock,
  running: Loader2,
  succeeded: CheckCircle2,
  failed: CircleAlert,
  cancelled: Ban,
};

const statusTone: Record<JobStatus, string> = {
  queued: 'text-muted-foreground',
  running: 'text-accent',
  succeeded: 'text-success',
  failed: 'text-danger',
  cancelled: 'text-muted-foreground',
};

/** Result summary stored by the export / import jobs (`result` is kind specific JSON). */
interface JobResult {
  path?: string | null;
  sizeBytes?: number;
  rows?: number;
  warnings?: string[];
  rowsRead?: number;
  inserted?: number;
  updated?: number;
  skipped?: number;
  errors?: { line: number; message: string }[];
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MiB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GiB`;
}

export function ProgressBar({ fraction, label }: { fraction: number | null; label: string }) {
  return (
    <div
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      {...(fraction === null ? {} : { 'aria-valuenow': Math.round(fraction * 100) })}
      className="h-1.5 w-full overflow-hidden rounded bg-surface-muted"
    >
      <div
        className={`h-full rounded bg-accent ${fraction === null ? 'w-1/3 animate-pulse' : ''}`}
        style={fraction === null ? undefined : { width: `${Math.round(fraction * 100)}%` }}
      />
    </div>
  );
}

function Summary({ job }: { job: JobInfo }): ReactNode {
  const { t } = useTranslation('jobs');
  const r = (job.result ?? {}) as JobResult;
  const parts: string[] = [];
  if (r.rows !== undefined) parts.push(t('result.rows', { count: r.rows }));
  if (r.inserted !== undefined) {
    parts.push(t('result.inserted', { count: r.inserted }));
    if (r.skipped) parts.push(t('result.skipped', { count: r.skipped }));
  }
  if (r.sizeBytes !== undefined) parts.push(formatBytes(r.sizeBytes));
  const errors = r.errors ?? [];
  return (
    <>
      {parts.length > 0 && <p data-testid="job-summary">{parts.join(' · ')}</p>}
      {r.path && <p className="select-text break-all text-muted-foreground">{r.path}</p>}
      {(r.warnings ?? []).map((w) => (
        <p key={w} className="text-warning">
          {w}
        </p>
      ))}
      {errors.length > 0 && (
        <ul className="mt-1 max-h-24 overflow-auto rounded bg-surface-muted p-1.5">
          {errors.slice(0, 20).map((e) => (
            <li key={`${e.line}-${e.message}`} className="select-text">
              {t('result.errorLine', { line: e.line })}: {e.message}
            </li>
          ))}
          {errors.length > 20 && <li>{t('result.moreErrors', { count: errors.length - 20 })}</li>}
        </ul>
      )}
    </>
  );
}

function JobRow({ job }: { job: JobInfo }) {
  const { t } = useTranslation('jobs');
  const Icon = statusIcon[job.status];
  const active = isActive(job);
  const fraction = jobFraction(job);
  const p = job.progress;
  return (
    <li data-testid={`job-${job.id}`} className="border-b border-border px-1 py-2 text-xs">
      <div className="flex items-center gap-2">
        <Icon
          size={14}
          aria-hidden
          className={`shrink-0 ${statusTone[job.status]} ${job.status === 'running' ? 'animate-spin' : ''}`}
        />
        <span className="min-w-0 flex-1 truncate text-[13px] font-medium" title={job.title}>
          {job.title}
        </span>
        <span data-testid="job-status" className={statusTone[job.status]}>
          {t(`status.${job.status}`)}
        </span>
        {active && job.cancellable && (
          <Button
            size="sm"
            variant="outline"
            onClick={() => void ipc.jobCancel(job.id).catch(() => undefined)}
          >
            {t('cancel')}
          </Button>
        )}
      </div>
      {job.status === 'running' && (
        <div className="mt-1.5 space-y-1">
          <ProgressBar fraction={fraction} label={t('progress')} />
          <p className="text-muted-foreground">
            {t(`phase.${p.phase}`, { defaultValue: p.phase })}
            {p.object ? ` · ${p.object}` : ''}
            {p.rowsDone > 0 ? ` · ${t('result.rows', { count: p.rowsDone })}` : ''}
            {fraction !== null ? ` · ${Math.round(fraction * 100)}%` : ''}
          </p>
        </div>
      )}
      {job.status === 'failed' && job.error && (
        <p className="mt-1 select-text break-words text-danger">{job.error.message}</p>
      )}
      {job.status === 'succeeded' && (
        <div className="mt-1 space-y-0.5">
          <Summary job={job} />
        </div>
      )}
    </li>
  );
}

/** The job center: every job of this session with progress, cancel and result. */
export function JobsPanel() {
  const { t } = useTranslation('jobs');
  const open = useJobsStore((s) => s.panelOpen);
  const setOpen = useJobsStore((s) => s.setPanelOpen);
  const jobs = useJobsStore((s) => s.jobs);
  const list = sortedJobs(jobs);
  const finished = list.filter((j) => !isActive(j)).length;
  const clear = async () => {
    await ipc.jobsClearFinished();
    useJobsStore.getState().setAll(Object.values(jobs).filter(isActive));
  };
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogContent
        title={t('title')}
        description={t('description')}
        className="w-[min(640px,94vw)]"
        footer={
          <Button disabled={finished === 0} onClick={() => void clear()}>
            <Trash2 size={13} />
            {t('clearFinished')}
          </Button>
        }
      >
        {list.length === 0 ? (
          <EmptyState title={t('empty.title')} description={t('empty.description')} />
        ) : (
          <ul>
            {list.map((j) => (
              <JobRow key={j.id} job={j} />
            ))}
          </ul>
        )}
      </DialogContent>
    </Dialog>
  );
}
