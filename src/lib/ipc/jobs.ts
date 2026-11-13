import { mock } from '../mock';
import type { JobId, JobInfo } from '../types';
import { call } from './core';

/** Background jobs; progress arrives through `ipc.onEvent('job://progress', ...)`. */
export const jobsApi = {
  jobsList: (): Promise<JobInfo[]> => call('jobs_list', () => mock.jobsList()),
  jobGet: (jobId: JobId): Promise<JobInfo> => call('job_get', () => mock.jobGet(jobId), { jobId }),
  jobCancel: (jobId: JobId): Promise<void> =>
    call('job_cancel', () => mock.jobCancel(jobId), { jobId }),
  /** Resolves to the number of removed jobs. */
  jobsClearFinished: (): Promise<number> =>
    call('jobs_clear_finished', () => mock.jobsClearFinished()),
};
