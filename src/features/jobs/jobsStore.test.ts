import { beforeEach, describe, expect, it } from 'vitest';
import { job, progress } from './testJobs';
import { isActive, jobFraction, sortedJobs, useJobsStore } from './jobsStore';

beforeEach(() => useJobsStore.setState({ jobs: {}, panelOpen: false }));

describe('jobsStore', () => {
  it('folds started / progress / finished events', () => {
    const { apply } = useJobsStore.getState();
    apply({ type: 'started', job: job('1') });
    apply({ type: 'progress', jobId: '1', progress: progress({ rowsDone: 5 }) });
    expect(useJobsStore.getState().jobs['1']?.progress.rowsDone).toBe(5);
    apply({ type: 'finished', job: job('1', { status: 'succeeded' }) });
    expect(useJobsStore.getState().jobs['1']?.status).toBe('succeeded');
  });

  it('ignores progress of unknown jobs and late progress of finished ones', () => {
    const { apply } = useJobsStore.getState();
    apply({ type: 'progress', jobId: 'x', progress: progress({ rowsDone: 1 }) });
    expect(Object.keys(useJobsStore.getState().jobs)).toHaveLength(0);
    apply({ type: 'finished', job: job('2', { status: 'cancelled' }) });
    apply({ type: 'progress', jobId: '2', progress: progress({ rowsDone: 9 }) });
    expect(useJobsStore.getState().jobs['2']?.progress.rowsDone).toBe(0);
  });

  it('sorts newest first and tells active jobs', () => {
    const { setAll } = useJobsStore.getState();
    setAll([job('1'), job('3', { status: 'queued' }), job('2', { status: 'failed' })]);
    const list = sortedJobs(useJobsStore.getState().jobs);
    expect(list.map((j) => j.id)).toEqual(['3', '2', '1']);
    expect(list.map(isActive)).toEqual([true, false, true]);
  });

  it('computes the fraction from rows, then bytes, then objects', () => {
    expect(jobFraction(job('1', { progress: progress({ rowsDone: 25, rowsTotal: 100 }) }))).toBe(
      0.25,
    );
    expect(jobFraction(job('1', { progress: progress({ bytesDone: 5, bytesTotal: 10 }) }))).toBe(
      0.5,
    );
    expect(jobFraction(job('1', { progress: progress({ objectsDone: 1, objectsTotal: 4 }) }))).toBe(
      0.25,
    );
    expect(jobFraction(job('1'))).toBeNull();
    expect(jobFraction(job('1', { status: 'succeeded' }))).toBe(1);
    expect(jobFraction(job('1', { progress: progress({ rowsDone: 500, rowsTotal: 100 }) }))).toBe(
      1,
    );
  });
});
