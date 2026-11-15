import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { resetMockConnections } from '@/lib/mock';
import type { ConnectionProfile } from '@/lib/types';
import { ConnectionDialog } from './ConnectionDialog';

vi.mock('@/lib/dialog', () => ({ pickFile: vi.fn(), pickSavePath: vi.fn() }));
vi.setConfig({ testTimeout: 30_000 });

beforeEach(async () => {
  resetMockConnections();
  vi.restoreAllMocks();
  await i18n.changeLanguage('en');
});

function setup(profile?: ConnectionProfile) {
  const user = userEvent.setup();
  render(
    <Providers>
      <ConnectionDialog open onOpenChange={vi.fn()} profile={profile} />
    </Providers>,
  );
  return { user, dialog: screen.getByRole('dialog') };
}

async function chooseAuth(
  user: ReturnType<typeof userEvent.setup>,
  dialog: HTMLElement,
  label: string,
) {
  await user.click(within(dialog).getByRole('combobox', { name: 'Authentication' }));
  await user.click(await screen.findByRole('option', { name: label }));
}

describe('ConnectionDialog authentication mode', () => {
  it('shows password fields by default and hides them without authentication', async () => {
    const { user, dialog } = setup();
    expect(within(dialog).getByLabelText('Password')).toBeInTheDocument();
    expect(within(dialog).getByText('Save password')).toBeInTheDocument();
    await chooseAuth(user, dialog, 'No authentication');
    expect(within(dialog).queryByLabelText('Password')).not.toBeInTheDocument();
    expect(within(dialog).queryByText('Save password')).not.toBeInTheDocument();
    // The user name stays, optional, with the usual default of the server as a hint.
    expect(within(dialog).getByLabelText(/Username/)).toHaveAttribute(
      'placeholder',
      'Default: postgres',
    );
    await chooseAuth(user, dialog, 'Username and password');
    expect(within(dialog).getByLabelText('Password')).toBeInTheDocument();
  });

  it('saves the profile without authentication and removes the stored secret', async () => {
    const save = vi.spyOn(ipc, 'connectionSave');
    const { user, dialog } = setup();
    await user.type(within(dialog).getByLabelText('Name'), 'Local Redis');
    await user.type(within(dialog).getByLabelText('Password'), 'pw');
    await chooseAuth(user, dialog, 'No authentication');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(save).toHaveBeenCalled());
    const [saved, secret] = save.mock.calls[0]!;
    expect(saved.authMode).toBe('none');
    expect(saved.savePassword).toBe(false);
    expect(secret).toEqual({ action: 'clear' });
  });

  it('tests the connection without sending the typed password', async () => {
    const test = vi
      .spyOn(ipc, 'connectionTest')
      .mockResolvedValue({ product: 'Redis', version: '7' } as never);
    const { user, dialog } = setup();
    await user.type(within(dialog).getByLabelText('Password'), 'pw');
    await chooseAuth(user, dialog, 'No authentication');
    await user.click(within(dialog).getByRole('button', { name: 'Test connection' }));
    await waitFor(() => expect(test).toHaveBeenCalled());
    expect(test.mock.calls[0]![0].authMode).toBe('none');
    expect(test.mock.calls[0]![1]).toEqual({ action: 'clear' });
  });
});
