import { create } from 'zustand';
import type { JobEvent, JobId, JobInfo } from '@/lib/types';

interface JobsState {
  jobs: Record<JobId, JobInfo>;
  /** Fold a `job://progress` event into the store. */
  apply: (event: JobEvent) => void;
  /** Replace the store content (initial `jobs_list`). */
  setAll: (jobs: JobInfo[]) => void;
}

export const useJobsStore = create<JobsState>((set) => ({
  jobs: {},
  apply: (event) =>
    set((s) => {
      switch (event.type) {
        case 'started':
        case 'finished':
          return { jobs: { ...s.jobs, [event.job.id]: event.job } };
        case 'progress': {
          const job = s.jobs[event.jobId];
          return job ? { jobs: { ...s.jobs, [job.id]: { ...job, progress: event.progress } } } : s;
        }
      }
    }),
  setAll: (jobs) => set({ jobs: Object.fromEntries(jobs.map((j) => [j.id, j])) }),
}));
