import { cleanup, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { renderWithProviders } from '@/features/data/testUtils';
import { ipc } from '@/lib/ipc';
import { mockAddJob, resetMockEvents } from '@/lib/mock';
import { resetMockJobs } from '@/lib/mock/jobs';
import { JobsIndicator } from './JobsIndicator';
import { useJobsStore } from './jobsStore';
import { job, progress } from './testJobs';

beforeEach(() => useJobsStore.setState({ jobs: {}, panelOpen: false }));
afterEach(() => {
  cleanup();
  resetMockJobs();
  resetMockEvents();
  vi.restoreAllMocks();
});

describe('JobsIndicator and the job center', () => {
  it('shows the running jobs of the backend and their progress', async () => {
    mockAddJob(
      job('1', {
        title: 'Export app → orders.xlsx',
        progress: progress({ rowsDone: 250, rowsTotal: 1000, object: 'public.orders' }),
      }),
    );
    renderWithProviders(<JobsIndicator />);
    const indicator = await screen.findByTestId('jobs-indicator');
    await waitFor(() => expect(indicator).toHaveTextContent('1 running'));
    expect(within(indicator).getByRole('progressbar')).toHaveAttribute('aria-valuenow', '25');

    await userEvent.setup().click(indicator);
    const row = await screen.findByTestId('job-1');
    expect(row).toHaveTextContent('Export app → orders.xlsx');
    expect(within(row).getByRole('progressbar')).toHaveAttribute('aria-valuenow', '25');
    expect(row).toHaveTextContent('public.orders');
  });

  it('follows job://progress events until the job finishes, with its result', async () => {
    renderWithProviders(<JobsIndicator />);
    const { mockEmit } = await import('@/lib/mock');
    await screen.findByTestId('jobs-indicator');
    await waitFor(() => expect(useJobsStore.getState().panelOpen).toBe(false));
    // Give the subscription a moment to register.
    await new Promise((r) => setTimeout(r, 20));
    mockEmit('job://progress', { type: 'started', job: job('2', { title: 'Import data.csv' }) });
    await waitFor(() =>
      expect(screen.getByTestId('jobs-indicator')).toHaveTextContent('1 running'),
    );
    mockEmit('job://progress', {
      type: 'finished',
      job: job('2', {
        title: 'Import data.csv',
        status: 'succeeded',
        result: {
          rowsRead: 3,
          inserted: 2,
          skipped: 1,
          errors: [{ line: 4, message: 'bad value' }],
        },
      }),
    });
    await userEvent.setup().click(screen.getByTestId('jobs-indicator'));
    const row = await screen.findByTestId('job-2');
    expect(within(row).getByTestId('job-status')).toHaveTextContent('Done');
    expect(row).toHaveTextContent('2 row(s) inserted');
    expect(row).toHaveTextContent('Line 4: bad value');
    expect(screen.getByTestId('jobs-indicator')).not.toHaveTextContent('running');
  });

  it('cancels a running job and clears the finished ones', async () => {
    const user = userEvent.setup();
    mockAddJob(job('3', { title: 'long export' }));
    mockAddJob(
      job('4', {
        title: 'old export',
        status: 'failed',
        error: { kind: 'query', message: 'boom' },
      }),
    );
    const cancel = vi.spyOn(ipc, 'jobCancel');
    const clear = vi.spyOn(ipc, 'jobsClearFinished');
    renderWithProviders(<JobsIndicator />);
    await user.click(await screen.findByTestId('jobs-indicator'));
    const failed = await screen.findByTestId('job-4');
    expect(failed).toHaveTextContent('boom');
    expect(within(failed).queryByRole('button', { name: 'Cancel' })).toBeNull();
    await user.click(within(screen.getByTestId('job-3')).getByRole('button', { name: 'Cancel' }));
    expect(cancel).toHaveBeenCalledWith('3');
    await user.click(screen.getByRole('button', { name: 'Clear finished' }));
    expect(clear).toHaveBeenCalled();
    await waitFor(() => expect(screen.queryByTestId('job-4')).toBeNull());
  });

  it('is empty without jobs', async () => {
    renderWithProviders(<JobsIndicator />);
    await userEvent.setup().click(await screen.findByTestId('jobs-indicator'));
    expect(await screen.findByText('No jobs yet')).toBeInTheDocument();
  });
});
