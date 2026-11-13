import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { resetMockConnections, resetMockEvents, resetMockPhase2 } from '@/lib/mock';
import { BackupJobDialog } from './BackupJobDialog';

beforeEach(async () => {
  resetMockPhase2();
  resetMockConnections();
  resetMockEvents();
  vi.restoreAllMocks();
  await i18n.changeLanguage('en');
});

function setup(props: Partial<Parameters<typeof BackupJobDialog>[0]> = {}) {
  const user = userEvent.setup();
  const onOpenChange = vi.fn();
  render(
    <Providers>
      <BackupJobDialog open onOpenChange={onOpenChange} {...props} />
    </Providers>,
  );
  return { user, onOpenChange };
}

async function pick(user: ReturnType<typeof userEvent.setup>, combo: string, option: string) {
  await user.click(await screen.findByRole('combobox', { name: combo }));
  await user.click(await screen.findByRole('option', { name: option }));
}

const tab = (name: string) => screen.getByRole('tab', { name });

describe('BackupJobDialog', () => {
  it('walks the sections, previews the schedule and saves the expected payload', async () => {
    const save = vi.spyOn(ipc, 'backupJobSave');
    const { user, onOpenChange } = setup();
    await user.type(await screen.findByLabelText('Job name'), 'Nightly app');
    await pick(user, 'Connection', 'PostgreSQL dev');

    await user.click(tab('Destination'));
    await user.type(screen.getByLabelText('Destination folder'), '/backups');
    expect(screen.getByTestId('file-example')).toHaveTextContent(
      /PostgreSQL_dev-app_dev-\d{8}-\d{6}\.dbsbak/,
    );

    await user.click(tab('Schedule'));
    expect(await screen.findByTestId('cron-description')).toHaveTextContent(/02:00/);
    await waitFor(() =>
      expect(within(screen.getByTestId('cron-preview')).getAllByRole('listitem')).toHaveLength(5),
    );

    await user.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    const [job, secret] = save.mock.calls[0]!;
    expect(secret).toEqual({ action: 'clear' });
    expect(job).toMatchObject({
      name: 'Nightly app',
      enabled: true,
      schedule: { cron: '0 2 * * *', timezone: 'local' },
      missedRun: 'runOnce',
      spec: {
        connectionId: '00000000-0000-4000-8000-0000000000b1',
        targets: [{ segments: [{ kind: 'database', name: 'app_dev' }] }],
        engine: { kind: 'logical', content: 'both' },
        compression: { kind: 'gzip', level: 6 },
        encrypted: false,
        destination: { kind: 'localDir', path: '/backups' },
      },
    });
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
    expect(await ipc.backupJobsList()).toHaveLength(1);
  });

  it('blocks saving and jumps to the first section with an error', async () => {
    const save = vi.spyOn(ipc, 'backupJobSave');
    const { user } = setup();
    await user.click(await screen.findByRole('button', { name: 'Save' }));
    expect(await screen.findByText('Enter a job name.')).toBeInTheDocument();
    expect(screen.getByText('Choose a connection.')).toBeInTheDocument();
    expect(save).not.toHaveBeenCalled();
  });

  it('suggests encryption for production connections and sends the passphrase', async () => {
    const save = vi.spyOn(ipc, 'backupJobSave');
    const { user } = setup();
    await user.type(await screen.findByLabelText('Job name'), 'Prod');
    await pick(user, 'Connection', 'PostgreSQL prod (read-only)');
    await user.click(tab('Destination'));
    await user.type(screen.getByLabelText('Destination folder'), '/backups');

    await user.click(tab('Format and security'));
    expect(screen.getByTestId('encrypt-suggestion')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Enable encryption' }));
    expect(screen.queryByTestId('encrypt-suggestion')).toBeNull();
    expect(screen.getByTestId('passphrase-warning')).toHaveTextContent(/lose the passphrase/);

    await user.type(screen.getByLabelText('Passphrase'), 'secret-1');
    await user.type(screen.getByLabelText('Repeat passphrase'), 'secret-2');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(await screen.findByText('The passphrases do not match.')).toBeInTheDocument();
    expect(save).not.toHaveBeenCalled();

    await user.clear(screen.getByLabelText('Repeat passphrase'));
    await user.type(screen.getByLabelText('Repeat passphrase'), 'secret-1');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    expect(save.mock.calls[0]![1]).toEqual({ action: 'set', value: 'secret-1' });
    expect(save.mock.calls[0]![0].spec.encrypted).toBe(true);
  });

  it('shows invalid cron expressions inline and refuses to save them', async () => {
    const save = vi.spyOn(ipc, 'backupJobSave');
    const { user } = setup();
    await user.type(await screen.findByLabelText('Job name'), 'Bad cron');
    await pick(user, 'Connection', 'PostgreSQL dev');
    await user.click(tab('Destination'));
    await user.type(screen.getByLabelText('Destination folder'), '/backups');
    await user.click(tab('Schedule'));
    await user.click(screen.getByRole('button', { name: 'Advanced (cron)' }));

    const input = screen.getByLabelText('Cron expression');
    await user.clear(input);
    await user.type(input, '* * *');
    expect(
      await screen.findByText('The cron expression must have 5 valid fields.'),
    ).toBeInTheDocument();

    // Right shape, rejected by the backend preview: error shown inline.
    await user.clear(input);
    await user.type(input, '99 * * * *');
    expect(await screen.findByText(/out of range/)).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(save).not.toHaveBeenCalled();

    await user.clear(input);
    await user.type(input, '*/15 * * * *');
    await waitFor(() => expect(screen.getByTestId('cron-preview')).toBeInTheDocument());
    expect(screen.getByTestId('cron-description')).toHaveTextContent(/15 minutes/);
  });

  it('runs a one-off backup without name or schedule', async () => {
    const run = vi.spyOn(ipc, 'backupRunManual');
    const { user, onOpenChange } = setup({ mode: 'once' });
    expect(screen.queryByRole('tab', { name: 'Schedule' })).toBeNull();
    await pick(user, 'Connection', 'PostgreSQL dev');
    await user.click(tab('Destination'));
    await user.type(screen.getByLabelText('Destination folder'), '/backups');
    await user.click(screen.getByRole('button', { name: 'Start backup' }));
    await waitFor(() => expect(run).toHaveBeenCalledTimes(1));
    expect(run.mock.calls[0]![0].destination).toMatchObject({ kind: 'localDir', path: '/backups' });
    expect(run.mock.calls[0]![1]).toBeNull();
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  });
});
