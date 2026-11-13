import type { JobInfo, JobProgress } from '@/lib/types';

export const progress = (p: Partial<JobProgress> = {}): JobProgress => ({
  phase: 'data',
  object: null,
  objectsDone: 0,
  objectsTotal: null,
  rowsDone: 0,
  rowsTotal: null,
  bytesDone: 0,
  bytesTotal: null,
  message: null,
  ...p,
});

export function job(id: string, p: Partial<JobInfo> = {}): JobInfo {
  return {
    id,
    kind: 'export',
    title: `job ${id}`,
    connectionId: null,
    status: 'running',
    progress: progress(),
    createdAt: `2026-01-01T00:00:0${id}Z`,
    startedAt: null,
    finishedAt: null,
    error: null,
    result: null,
    cancellable: true,
    ...p,
  };
}
