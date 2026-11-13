import { useEffect } from 'react';
import { create } from 'zustand';
import { ipc } from '@/lib/ipc';
import { EVENTS } from '@/lib/ipc/events';
import type { JobEvent, JobId, JobInfo } from '@/lib/types';

interface JobsState {
  jobs: Record<JobId, JobInfo>;
  /** The jobs panel is open. */
  panelOpen: boolean;
  setPanelOpen: (open: boolean) => void;
  /** Fold a `job://progress` event into the store. */
  apply: (event: JobEvent) => void;
  /** Replace the store content (initial `jobs_list`). */
  setAll: (jobs: JobInfo[]) => void;
}

export const useJobsStore = create<JobsState>((set) => ({
  jobs: {},
  panelOpen: false,
  setPanelOpen: (panelOpen) => set({ panelOpen }),
  apply: (event) =>
    set((s) => {
      switch (event.type) {
        case 'started':
        case 'finished':
          return { jobs: { ...s.jobs, [event.job.id]: event.job } };
        case 'progress': {
          const job = s.jobs[event.jobId];
          if (!job) return s;
          // A late progress event must not bring a finished job back to life.
          if (job.status !== 'running' && job.status !== 'queued') return s;
          return { jobs: { ...s.jobs, [job.id]: { ...job, progress: event.progress } } };
        }
      }
    }),
  setAll: (jobs) => set({ jobs: Object.fromEntries(jobs.map((j) => [j.id, j])) }),
}));

export function isActive(job: JobInfo): boolean {
  return job.status === 'queued' || job.status === 'running';
}

/** Jobs newest first. */
export function sortedJobs(jobs: Record<JobId, JobInfo>): JobInfo[] {
  return Object.values(jobs).sort(
    (a, b) => b.createdAt.localeCompare(a.createdAt) || b.id.localeCompare(a.id),
  );
}

/** Completion 0..1 of a job, `null` when it cannot be told. */
export function jobFraction(job: JobInfo): number | null {
  if (job.status === 'succeeded') return 1;
  const { rowsDone, rowsTotal, bytesDone, bytesTotal, objectsDone, objectsTotal } = job.progress;
  const pick = (done: number, total: number | null) =>
    total && total > 0 ? Math.min(1, done / total) : null;
  return (
    pick(rowsDone, rowsTotal) ?? pick(bytesDone, bytesTotal) ?? pick(objectsDone, objectsTotal)
  );
}

/** Keeps the store in sync with the backend while mounted (list once, then events). */
export function useJobsSync(): void {
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    const { apply, setAll } = useJobsStore.getState();
    void ipc.onEvent<JobEvent>(EVENTS.jobProgress, apply).then((off) => {
      if (cancelled) off();
      else unlisten = off;
    });
    void ipc
      .jobsList()
      .then((list) => {
        if (cancelled) return;
        // Events that arrived before the list keep their newer state.
        const current = useJobsStore.getState().jobs;
        setAll(list.map((j) => current[j.id] ?? j));
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);
}
