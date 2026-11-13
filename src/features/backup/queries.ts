import { useQueryClient, useQuery } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { ipc, EVENTS } from '@/lib/ipc';
import type { ScheduleTz } from '@/lib/types';
import { isCronShape } from './schedule';
import type { JobEvent, JobId, JobInfo, JobKind } from '@/lib/types';

export const backupJobsKey = ['backup', 'jobs'] as const;
export const backupRunsKey = ['backup', 'runs'] as const;
export const backupToolsKey = ['backup', 'tools'] as const;
export const backgroundKey = ['backup', 'background'] as const;

const BACKUP_KINDS: readonly JobKind[] = ['backup', 'restore', 'verify'];

export function useBackupJobsQuery() {
  return useQuery({ queryKey: backupJobsKey, queryFn: ipc.backupJobsList });
}

export function useToolsQuery() {
  return useQuery({ queryKey: backupToolsKey, queryFn: ipc.backupToolsDetect, staleTime: 30_000 });
}

/**
 * Tracks backup/restore/verify jobs from `job://progress` and refreshes the job and run lists on
 * `backup://changed` and when a job finishes. Returns every job seen since mount, by id.
 */
export function useBackupEvents(): Record<JobId, JobInfo> {
  const qc = useQueryClient();
  const [jobs, setJobs] = useState<Record<JobId, JobInfo>>({});

  useEffect(() => {
    let disposed = false;
    const unsubs: (() => void)[] = [];
    const refresh = () => {
      void qc.invalidateQueries({ queryKey: ['backup', 'jobs'] });
      void qc.invalidateQueries({ queryKey: ['backup', 'runs'] });
    };
    const keep = (p: Promise<() => void>) =>
      p.then((u) => (disposed ? u() : unsubs.push(u))).catch(() => undefined);
    keep(ipc.onEvent<null>(EVENTS.backupChanged, refresh));
    keep(
      ipc.onEvent<JobEvent>(EVENTS.jobProgress, (event) => {
        setJobs((prev) => foldEvent(prev, event));
        if (event.type === 'finished' && BACKUP_KINDS.includes(event.job.kind)) refresh();
      }),
    );
    return () => {
      disposed = true;
      unsubs.forEach((u) => u());
    };
  }, [qc]);

  return jobs;
}

/** Fold an event into a map of backup-related jobs (other job kinds are ignored). */
export function foldEvent(prev: Record<JobId, JobInfo>, event: JobEvent): Record<JobId, JobInfo> {
  switch (event.type) {
    case 'started':
    case 'finished':
      return BACKUP_KINDS.includes(event.job.kind) ? { ...prev, [event.job.id]: event.job } : prev;
    case 'progress': {
      const job = prev[event.jobId];
      return job ? { ...prev, [job.id]: { ...job, progress: event.progress } } : prev;
    }
  }
}

export function isActive(job: JobInfo): boolean {
  return job.status === 'queued' || job.status === 'running';
}

export const PREVIEW_COUNT = 5;

/** Next runs of a cron expression (shared by the builder and the dialog's validation). */
export function useSchedulePreview(cron: string, timezone: ScheduleTz) {
  const normalized = cron.trim().replace(/\s+/g, ' ');
  return useQuery({
    queryKey: ['backup', 'schedulePreview', normalized, timezone],
    queryFn: () => ipc.schedulePreview(normalized, timezone, PREVIEW_COUNT),
    enabled: isCronShape(normalized),
    staleTime: 30_000,
  });
}
