import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { resetMockConnections } from '@/lib/mock';
import { ConnectionDialog } from './ConnectionDialog';

vi.mock('@/lib/dialog', () => ({ pickFile: vi.fn(), pickSavePath: vi.fn() }));
vi.setConfig({ testTimeout: 30_000 });

beforeEach(async () => {
  resetMockConnections();
  await i18n.changeLanguage('en');
});
afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

function setup() {
  const user = userEvent.setup();
  render(
    <Providers>
      <ConnectionDialog open onOpenChange={vi.fn()} />
    </Providers>,
  );
  return { user, dialog: screen.getByRole('dialog') };
}

describe('ConnectionDialog feedback', () => {
  it('focuses the first invalid field when Save is rejected', async () => {
    const { user, dialog } = setup();
    const host = within(dialog).getByLabelText('Host');
    await user.clear(host);
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(host).toHaveFocus());
    expect(within(dialog).getByText('Host is required')).toBeInTheDocument();
    expect(within(dialog).queryByText('Name is required')).not.toBeInTheDocument();
  });

  it('saves a blank name as the suggested `<kind> N` name', async () => {
    const save = vi.spyOn(ipc, 'connectionSave');
    const { user, dialog } = setup();
    const name = within(dialog).getByLabelText('Name');
    await waitFor(() => expect(name).toHaveAttribute('placeholder', 'postgres 1'));
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(save).toHaveBeenCalled());
    expect(save.mock.calls[0]![0].name).toBe('postgres 1');
  });

  it('keeps the name the user typed', async () => {
    const save = vi.spyOn(ipc, 'connectionSave');
    const { user, dialog } = setup();
    await user.type(within(dialog).getByLabelText('Name'), 'Kho dữ liệu');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(save).toHaveBeenCalled());
    expect(save.mock.calls[0]![0].name).toBe('Kho dữ liệu');
  });

  it('leaves the name blank after a URI and suggests one for the new kind', async () => {
    const { user, dialog } = setup();
    await user.type(
      within(dialog).getByLabelText('Connection URI'),
      'mongodb://u@db.example.com:27017/shop',
    );
    await user.click(within(dialog).getByRole('button', { name: 'Fill from URI' }));
    const name = within(dialog).getByLabelText('Name');
    await waitFor(() => expect(name).toHaveAttribute('placeholder', 'mongodb 1'));
    expect(name).toHaveValue('');
  });

  it('explains a slow test that may wait on the keychain', async () => {
    vi.spyOn(ipc, 'connectionTest').mockReturnValue(new Promise(() => undefined));
    const { user, dialog } = setup();
    vi.useFakeTimers({ shouldAdvanceTime: true });
    await user.click(within(dialog).getByRole('button', { name: 'Test connection' }));
    expect(screen.queryByText(/Keychain/)).not.toBeInTheDocument();
    await vi.advanceTimersByTimeAsync(3100);
    expect(await screen.findByText(/Still waiting|Keychain/)).toBeInTheDocument();
  });
});
