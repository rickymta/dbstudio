import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { resetMockConnections, resetMockEvents, resetMockPhase2 } from '@/lib/mock';
import { MOCK_PASSPHRASE } from '@/lib/mock/backup';
import { ConfirmDestructiveHost } from '@/features/safety/ConfirmDestructiveHost';
import { RestoreDialog } from './RestoreDialog';

beforeEach(async () => {
  resetMockPhase2();
  resetMockConnections();
  resetMockEvents();
  vi.restoreAllMocks();
  await i18n.changeLanguage('en');
});

async function setup(file: string) {
  vi.spyOn(ipc, 'filePickOpen').mockResolvedValue(file);
  const user = userEvent.setup();
  render(
    <Providers>
      <RestoreDialog open onOpenChange={() => {}} />
      <ConfirmDestructiveHost />
    </Providers>,
  );
  await user.click(await screen.findByRole('button', { name: /Choose \.dbsbak file/ }));
  return user;
}

async function pickTarget(user: ReturnType<typeof userEvent.setup>, name: string) {
  await user.click(await screen.findByRole('combobox', { name: 'Target connection' }));
  await user.click(await screen.findByRole('option', { name }));
}

/** A writable production profile (the seeded production one is read-only). */
async function addProduction() {
  const { profiles } = await ipc.connectionsList();
  await ipc.connectionSave(
    {
      ...profiles[0]!,
      id: '00000000-0000-4000-8000-0000000000c9',
      name: 'Prod writable',
      environment: 'production',
      readOnly: false,
    },
    { action: 'keep' },
  );
}

describe('RestoreDialog', () => {
  it('asks for the passphrase of an encrypted file and reports a wrong one as an integrity error', async () => {
    const user = await setup('/backups/shop-enc.dbsbak');
    const pass = await screen.findByLabelText('Passphrase');
    expect(screen.getByText(/Enter the passphrase to read its contents/)).toBeInTheDocument();
    expect(screen.queryByTestId('restore-header')).toBeNull();

    await user.type(pass, 'wrong');
    await user.click(screen.getByRole('button', { name: 'Unlock' }));
    expect(
      await screen.findByText(/Wrong passphrase, or the file is corrupted/),
    ).toBeInTheDocument();
    expect(screen.queryByTestId('restore-header')).toBeNull();

    await user.clear(pass);
    await user.type(pass, MOCK_PASSPHRASE);
    await user.click(screen.getByRole('button', { name: 'Unlock' }));
    const header = await screen.findByTestId('restore-header');
    expect(within(header).getByText('shop')).toBeInTheDocument();
    expect(screen.getByText('shop.customers')).toBeInTheDocument();
    expect(screen.queryByText(/Wrong passphrase/)).toBeNull();
  });

  it('restores an integrated backup as a whole into a development database', async () => {
    const start = vi.spyOn(ipc, 'restoreStart');
    const user = await setup('/backups/shop.dbsbak');
    expect(await screen.findByTestId('restore-header')).toBeInTheDocument();
    // Integrated backups list their objects but cannot be restored selectively.
    expect(screen.queryByLabelText('shop.orders')).toBeNull();
    expect(screen.getByText('shop.orders')).toBeInTheDocument();
    await pickTarget(user, 'PostgreSQL dev');
    await user.click(screen.getByRole('button', { name: 'Restore' }));
    await waitFor(() => expect(start).toHaveBeenCalledTimes(1));
    expect(start.mock.calls[0]![0]).toMatchObject({
      file: '/backups/shop.dbsbak',
      targetConnectionId: '00000000-0000-4000-8000-0000000000b1',
      targetDatabase: 'shop',
    });
    // An integrated backup is restored as a whole: no object subset is sent.
    expect(start.mock.calls[0]![0]).not.toHaveProperty('objects');
    expect(start.mock.calls[0]![2]).toBeNull();
    expect(await screen.findByText('Succeeded', undefined, { timeout: 3000 })).toBeInTheDocument();
  });

  it('asks to retype the confirmation text before restoring into production', async () => {
    await addProduction();
    const start = vi.spyOn(ipc, 'restoreStart');
    const user = await setup('/backups/shop.dbsbak');
    await screen.findByTestId('restore-header');
    await pickTarget(user, 'Prod writable');
    expect(screen.getByTestId('restore-production')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Restore' }));

    const dialog = await screen.findByRole('dialog', { name: 'Confirm destructive statements' });
    const confirm = within(dialog).getByRole('button', { name: 'Run anyway' });
    expect(confirm).toBeDisabled();
    await user.type(within(dialog).getByRole('textbox'), 'shop');
    await user.click(confirm);

    await waitFor(() => expect(start).toHaveBeenCalledTimes(2));
    expect(start.mock.calls[0]![2]).toBeNull();
    expect(start.mock.calls[1]![2]).toMatchObject({ typed: 'shop' });
    expect(start.mock.calls[1]![2]!.token).toBeTruthy();
    await waitFor(() =>
      expect(screen.queryByRole('dialog', { name: 'Confirm destructive statements' })).toBeNull(),
    );
  });

  it('does not let a production or clean restore run without a named target database', async () => {
    await addProduction();
    const start = vi.spyOn(ipc, 'restoreStart');
    const user = await setup('/backups/shop.dbsbak');
    await screen.findByTestId('restore-header');
    await pickTarget(user, 'Prod writable');
    const db = screen.getByLabelText('Target database');
    await user.clear(db);
    expect(screen.getByTestId('restore-database-required')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Restore' })).toBeDisabled();
    await user.type(db, 'shop_copy');
    expect(screen.queryByTestId('restore-database-required')).toBeNull();
    expect(screen.getByRole('button', { name: 'Restore' })).toBeEnabled();
    // `clean` has the same rule on any target.
    await pickTarget(user, 'PostgreSQL dev');
    await user.clear(db);
    expect(screen.getByRole('button', { name: 'Restore' })).toBeEnabled();
    await user.click(screen.getByLabelText('Drop existing objects first (clean)'));
    expect(screen.getByRole('button', { name: 'Restore' })).toBeDisabled();
    expect(start).not.toHaveBeenCalled();
  });

  it('says what clean and a MySQL restore really do', async () => {
    const user = await setup('/backups/shop.dbsbak');
    await screen.findByTestId('restore-header');
    expect(screen.getByTestId('restore-not-atomic')).toHaveTextContent(
      /cannot be rolled back|partly restored/,
    );
    await user.click(screen.getByLabelText('Drop existing objects first (clean)'));
    const note = screen.getByRole('note');
    expect(note).toHaveTextContent(/CASCADE/);
    expect(note).toHaveTextContent(/cannot be rolled back/);
  });

  it('warns about the destructive clean option', async () => {
    const user = await setup('/backups/shop.dbsbak');
    await screen.findByTestId('restore-header');
    await user.click(screen.getByLabelText('Drop existing objects first (clean)'));
    expect(screen.getByRole('note')).toHaveTextContent(/Destructive/);
  });
});
