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
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    const name = within(dialog).getByLabelText('Name');
    await waitFor(() => expect(name).toHaveFocus());
    expect(within(dialog).getByText('Name is required')).toBeInTheDocument();
  });

  it('fills a default name from a URI when the name is empty', async () => {
    const { user, dialog } = setup();
    await user.type(
      within(dialog).getByLabelText('Connection URI'),
      'mongodb://u@db.example.com:27017/shop',
    );
    await user.click(within(dialog).getByRole('button', { name: 'Fill from URI' }));
    await waitFor(() =>
      expect(within(dialog).getByLabelText('Name')).toHaveValue('mongodb · shop'),
    );
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
