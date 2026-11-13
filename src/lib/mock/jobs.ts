// Mock background jobs: the list is empty; mocks that create jobs push into `mockJobs` and emit
// `job://progress` events through `mockEmit`.

import type { JobId, JobInfo } from '../types';
import { mockEmit } from './events';
import { clone, ipcError } from './util';

const jobs: JobInfo[] = [];

/** Register a job and announce it (for mocks of export / backup commands). */
export function mockAddJob(job: JobInfo): void {
  jobs.push(clone(job));
  mockEmit('job://progress', { type: 'started', job: clone(job) });
}

export const jobsMock = {
  async jobsList(): Promise<JobInfo[]> {
    return clone(jobs);
  },
  async jobGet(jobId: JobId): Promise<JobInfo> {
    const job = jobs.find((j) => j.id === jobId);
    if (!job) throw ipcError('notFound', `job ${jobId}`);
    return clone(job);
  },
  async jobCancel(jobId: JobId): Promise<void> {
    const job = jobs.find((j) => j.id === jobId);
    if (!job) throw ipcError('notFound', `job ${jobId}`);
    if (job.status === 'queued' || job.status === 'running') {
      job.status = 'cancelled';
      mockEmit('job://progress', { type: 'finished', job: clone(job) });
    }
  },
  async jobsClearFinished(): Promise<number> {
    const before = jobs.length;
    for (let i = jobs.length - 1; i >= 0; i--) {
      if (!['queued', 'running'].includes(jobs[i]!.status)) jobs.splice(i, 1);
    }
    return before - jobs.length;
  },
};

export function resetMockJobs(): void {
  jobs.length = 0;
}
