import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { resetMockConnections, resetMockEvents, resetMockPhase2 } from '@/lib/mock';
import type { BackupJob } from '@/lib/types';
import { BackupTab } from './BackupTab';

const CONNECTION = '00000000-0000-4000-8000-0000000000b1';

function job(over: Partial<BackupJob> = {}): BackupJob {
  return {
    id: 'job-1',
    name: 'Nightly app',
    enabled: true,
    spec: {
      connectionId: CONNECTION,
      targets: [{ segments: [{ kind: 'database', name: 'app_dev' }] }],
      engine: { kind: 'logical', content: 'both' },
      destination: { kind: 'localDir', path: '/backups' },
    },
    schedule: { cron: '0 2 * * *', timezone: 'utc' },
    lastRunAt: null,
    nextRunAt: null,
    createdAt: '2026-01-01T00:00:00Z',
    updatedAt: '2026-01-01T00:00:00Z',
    ...over,
  };
}

beforeEach(async () => {
  resetMockPhase2();
  resetMockConnections();
  resetMockEvents();
  vi.restoreAllMocks();
  await i18n.changeLanguage('en');
});

function setup() {
  const user = userEvent.setup();
  render(
    <Providers>
      <BackupTab />
    </Providers>,
  );
  return user;
}

describe('BackupTab', () => {
  it('shows an empty state without jobs', async () => {
    setup();
    expect(await screen.findByText('No backup jobs yet')).toBeInTheDocument();
  });

  it('lists jobs with a described schedule and runs one now with live progress', async () => {
    await ipc.backupJobSave(job(), { action: 'clear' });
    const run = vi.spyOn(ipc, 'backupJobRunNow');
    const user = setup();
    const row = await screen.findByTestId('job-row');
    expect(within(row).getByText('Nightly app')).toBeInTheDocument();
    expect(await within(row).findByText('PostgreSQL dev')).toBeInTheDocument();
    // The schedule description and the next run time can both read 02:00 (e.g. in UTC).
    expect(within(row).getAllByText(/02:00/).length).toBeGreaterThan(0);
    expect(within(row).getByText(/UTC/)).toBeInTheDocument();

    await user.click(within(row).getByRole('button', { name: 'Run now' }));
    expect(run).toHaveBeenCalledWith('job-1');
    expect(await screen.findByTestId('running-jobs')).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByTestId('running-jobs')).toBeNull(), { timeout: 3000 });

    await user.click(screen.getByRole('tab', { name: 'Run history' }));
    const runRow = await screen.findByTestId('run-row');
    expect(within(runRow).getByText('Succeeded')).toBeInTheDocument();
    expect(within(runRow).getByText('Nightly app')).toBeInTheDocument();
    expect(within(runRow).getByRole('button', { name: /Verify integrity/ })).toBeInTheDocument();
  });

  it('disables a job through the switch and deletes it after confirmation', async () => {
    await ipc.backupJobSave(job(), { action: 'clear' });
    const save = vi.spyOn(ipc, 'backupJobSave');
    const user = setup();
    const row = await screen.findByTestId('job-row');
    await user.click(within(row).getByRole('switch', { name: 'Enable job Nightly app' }));
    await waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    expect(save.mock.calls[0]![0].enabled).toBe(false);
    expect(save.mock.calls[0]![1]).toEqual({ action: 'keep' });

    await user.click(within(row).getByRole('button', { name: 'Delete' }));
    const dialog = await screen.findByRole('dialog', { name: 'Delete job "Nightly app"?' });
    expect(dialog).toHaveTextContent(/files it already created are kept/);
    await user.click(within(dialog).getByRole('button', { name: 'Delete' }));
    await waitFor(async () => expect(await ipc.backupJobsList()).toHaveLength(0));
    expect(await screen.findByText('No backup jobs yet')).toBeInTheDocument();
  });

  it('shows native tools and the problem of a missing one, and sets a path', async () => {
    vi.spyOn(ipc, 'filePickOpen').mockResolvedValue('/opt/bin/mysqldump');
    const user = setup();
    await user.click(await screen.findByRole('tab', { name: 'Native tools' }));
    const pg = await screen.findByTestId('tool-pgDump');
    expect(within(pg).getByText('Found')).toBeInTheDocument();
    const my = screen.getByTestId('tool-mySqlDump');
    expect(within(my).getByText('not found')).toBeInTheDocument();

    await user.click(within(my).getByRole('button', { name: /Choose path/ }));
    await waitFor(() =>
      expect(within(screen.getByTestId('tool-mySqlDump')).getByText('Found')).toBeInTheDocument(),
    );
    expect(screen.getByTestId('tool-mySqlDump')).toHaveTextContent('/opt/bin/mysqldump');
    await user.click(
      within(screen.getByTestId('tool-mySqlDump')).getByRole('button', { name: /Clear setting/ }),
    );
    await waitFor(() =>
      expect(within(screen.getByTestId('tool-mySqlDump')).getByText('Missing')).toBeInTheDocument(),
    );
  });

  it('saves the background settings', async () => {
    const set = vi.spyOn(ipc, 'backgroundSet');
    const user = setup();
    await user.click(await screen.findByRole('tab', { name: 'Background' }));
    await user.click(await screen.findByRole('switch', { name: 'Start DBStudio when I sign in' }));
    await waitFor(() => expect(set).toHaveBeenCalledWith({ closeToTray: 'auto', autostart: true }));
    await user.click(screen.getByRole('combobox', { name: 'When the window is closed' }));
    await user.click(
      await screen.findByRole('option', { name: 'Always keep running in the tray' }),
    );
    await waitFor(() =>
      expect(set).toHaveBeenLastCalledWith({ closeToTray: 'always', autostart: true }),
    );
  });

  it('opens the create-job and restore dialogs', async () => {
    const user = setup();
    await user.click(await screen.findByRole('button', { name: 'Restore from file...' }));
    expect(await screen.findByRole('dialog', { name: 'Restore from backup' })).toBeInTheDocument();
    await user.keyboard('{Escape}');
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    await user.click(screen.getAllByRole('button', { name: 'Create job' })[0]!);
    expect(await screen.findByRole('dialog', { name: 'Create backup job' })).toBeInTheDocument();
  });
});
