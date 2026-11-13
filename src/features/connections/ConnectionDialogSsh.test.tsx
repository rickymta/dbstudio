import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { resetMockConnections } from '@/lib/mock';
import type { IpcError, ServerInfo } from '@/lib/types';
import { ConnectionDialog } from './ConnectionDialog';

vi.mock('@/lib/dialog', () => ({ pickFile: vi.fn(), pickSavePath: vi.fn() }));

// Typing into many fields is slow under load.
vi.setConfig({ testTimeout: 30_000 });

beforeEach(async () => {
  resetMockConnections();
  vi.restoreAllMocks();
  await i18n.changeLanguage('en');
});

const INFO: ServerInfo = { product: 'PostgreSQL', version: '16.1' } as ServerInfo;

const HOST_KEY: IpcError = {
  kind: 'sshHostKey',
  message: 'bastion',
  host: 'bastion',
  port: 22,
  keyType: 'ssh-ed25519',
  fingerprint: 'SHA256:zzz',
  changed: false,
};

function setup() {
  const user = userEvent.setup();
  const onOpenChange = vi.fn();
  render(
    <Providers>
      <ConnectionDialog open onOpenChange={onOpenChange} />
    </Providers>,
  );
  return { user, onOpenChange };
}

async function fillBasics(user: ReturnType<typeof userEvent.setup>, dialog: HTMLElement) {
  await user.type(within(dialog).getByLabelText('Name'), 'Behind bastion');
  await user.clear(within(dialog).getByLabelText('Host'));
  await user.type(within(dialog).getByLabelText('Host'), 'db.internal');
}

async function enableTunnel(user: ReturnType<typeof userEvent.setup>, dialog: HTMLElement) {
  await user.click(within(dialog).getByRole('switch', { name: 'Connect through an SSH tunnel' }));
  await user.type(within(dialog).getByLabelText('SSH host'), 'bastion.example.com');
  await user.type(within(dialog).getByLabelText('SSH user'), 'deploy');
  await user.type(within(dialog).getByLabelText('SSH password'), 'tunnel-pw');
}

describe('ConnectionDialog SSH tunnel', () => {
  it('saves the tunnel settings and the SSH secret separately from the DB password', async () => {
    const save = vi.spyOn(ipc, 'connectionSave');
    const { user, onOpenChange } = setup();
    const dialog = await screen.findByRole('dialog', { name: 'New connection' });
    await fillBasics(user, dialog);
    await enableTunnel(user, dialog);
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    const [profile, secret, sshSecret] = save.mock.calls[0]!;
    expect(profile.ssh).toEqual({
      host: 'bastion.example.com',
      port: 22,
      username: 'deploy',
      auth: { method: 'password' },
      keepaliveSecs: null,
      connectTimeoutMs: null,
    });
    expect(secret).toEqual({ action: 'keep' });
    expect(sshSecret).toEqual({ action: 'set', value: 'tunnel-pw' });
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  });

  it('does not pass an SSH secret and leaves out ssh while the tunnel is off', async () => {
    const save = vi.spyOn(ipc, 'connectionSave');
    const { user } = setup();
    const dialog = await screen.findByRole('dialog', { name: 'New connection' });
    await fillBasics(user, dialog);
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    expect(save.mock.calls[0]!.length).toBe(2);
    expect('ssh' in save.mock.calls[0]![0]).toBe(false);
  });

  it('refuses to save an incomplete tunnel and shows why', async () => {
    const save = vi.spyOn(ipc, 'connectionSave');
    const { user } = setup();
    const dialog = await screen.findByRole('dialog', { name: 'New connection' });
    await fillBasics(user, dialog);
    await user.click(within(dialog).getByRole('switch', { name: 'Connect through an SSH tunnel' }));
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    expect(await within(dialog).findByText('Enter the SSH host.')).toBeInTheDocument();
    expect(within(dialog).getByText('Enter the SSH user.')).toBeInTheDocument();
    expect(save).not.toHaveBeenCalled();
  });

  it('asks to trust an unknown host key during the connection test and retries', async () => {
    const test = vi
      .spyOn(ipc, 'connectionTest')
      .mockRejectedValueOnce(HOST_KEY)
      .mockResolvedValueOnce(INFO);
    const trust = vi.spyOn(ipc, 'sshTrustHost').mockResolvedValue();
    const { user } = setup();
    const dialog = await screen.findByRole('dialog', { name: 'New connection' });
    await fillBasics(user, dialog);
    await enableTunnel(user, dialog);
    await user.click(within(dialog).getByRole('button', { name: 'Test connection' }));
    const hostKey = await screen.findByRole('dialog', { name: 'Trust this SSH server?' });
    expect(within(hostKey).getByTestId('host-key-fingerprint')).toHaveTextContent('SHA256:zzz');
    await user.click(within(hostKey).getByRole('button', { name: 'Trust and continue' }));
    await waitFor(() => expect(test).toHaveBeenCalledTimes(2));
    expect(trust).toHaveBeenCalledWith('bastion', 22, 'ssh-ed25519', 'SHA256:zzz');
    expect(await within(dialog).findByRole('status')).toHaveTextContent('16.1');
    // The tunnel settings were part of the tested profile.
    expect(test.mock.calls[1]![0].ssh).toMatchObject({ host: 'bastion.example.com' });
    // The SSH password typed in the dialog is sent so the test works before saving.
    expect(test.mock.calls[1]![2]).toEqual({ action: 'set', value: 'tunnel-pw' });
  });

  it('shows the host key error when the user declines', async () => {
    vi.spyOn(ipc, 'connectionTest').mockRejectedValue(HOST_KEY);
    const trust = vi.spyOn(ipc, 'sshTrustHost').mockResolvedValue();
    const { user } = setup();
    const dialog = await screen.findByRole('dialog', { name: 'New connection' });
    await fillBasics(user, dialog);
    await enableTunnel(user, dialog);
    await user.click(within(dialog).getByRole('button', { name: 'Test connection' }));
    const hostKey = await screen.findByRole('dialog', { name: 'Trust this SSH server?' });
    await user.click(within(hostKey).getByRole('button', { name: 'Cancel' }));
    expect(await within(dialog).findByText(/SSH host key/)).toBeInTheDocument();
    expect(trust).not.toHaveBeenCalled();
  });
});
