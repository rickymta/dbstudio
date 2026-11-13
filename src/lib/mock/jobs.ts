// Mock background jobs: the list is empty; mocks that create jobs push into `mockJobs` and emit
// `job://progress` events through `mockEmit`.

import type { JobId, JobInfo, JobKind } from '../types';
import { mockEmit } from './events';
import { clone, ipcError, newId } from './util';

const jobs: JobInfo[] = [];

/** Register a job and announce it (for mocks of export / backup commands). */
export function mockAddJob(job: JobInfo): void {
  jobs.push(clone(job));
  mockEmit('job://progress', { type: 'started', job: clone(job) });
}

/** Milliseconds between the progress steps of a simulated job (tests lower it). */
export const mockJobTiming = { stepMs: 120, steps: 5 };

/**
 * Create a job that advances by itself: `steps` progress events, then `finished` with `result`
 * (or a failure when `fail` is given). Cancelling it stops the timer.
 */
export function mockRunJob(options: {
  kind: JobKind;
  title: string;
  connectionId: string | null;
  rowsTotal: number;
  result: unknown;
  fail?: { kind: string; message: string };
}): JobId {
  const now = new Date().toISOString();
  const job: JobInfo = {
    id: newId(),
    kind: options.kind,
    title: options.title,
    connectionId: options.connectionId as JobInfo['connectionId'],
    status: 'running',
    progress: {
      phase: 'data',
      object: null,
      objectsDone: 0,
      objectsTotal: 1,
      rowsDone: 0,
      rowsTotal: options.rowsTotal,
      bytesDone: 0,
      bytesTotal: null,
      message: null,
    },
    createdAt: now,
    startedAt: now,
    finishedAt: null,
    error: null,
    result: null,
    cancellable: true,
  };
  mockAddJob(job);
  const stored = jobs[jobs.length - 1]!;
  let step = 0;
  const timer = setInterval(() => {
    if (stored.status !== 'running') {
      clearInterval(timer);
      return;
    }
    step += 1;
    if (step < mockJobTiming.steps) {
      stored.progress = {
        ...stored.progress,
        rowsDone: Math.round((options.rowsTotal * step) / mockJobTiming.steps),
      };
      mockEmit('job://progress', {
        type: 'progress',
        jobId: stored.id,
        progress: clone(stored.progress),
      });
      return;
    }
    clearInterval(timer);
    stored.finishedAt = new Date().toISOString();
    if (options.fail) {
      stored.status = 'failed';
      stored.error = options.fail;
    } else {
      stored.status = 'succeeded';
      stored.progress = { ...stored.progress, rowsDone: options.rowsTotal };
      stored.result = options.result;
    }
    mockEmit('job://progress', { type: 'finished', job: clone(stored) });
  }, mockJobTiming.stepMs);
  return stored.id;
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
