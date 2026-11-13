import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import { pickFile } from '@/lib/dialog';
import i18n from '@/lib/i18n';
import type { ConnectionProfile, SshConfig } from '@/lib/types';
import { SshSection } from './SshSection';
import { initialSshForm, validateSsh, type SshFormState } from './sshForm';

vi.mock('@/lib/dialog', () => ({ pickFile: vi.fn(), pickSavePath: vi.fn() }));

beforeEach(async () => {
  vi.mocked(pickFile).mockReset();
  await i18n.changeLanguage('en');
});

function profileWith(ssh: SshConfig): ConnectionProfile {
  return {
    id: 'c',
    name: 'n',
    kind: 'postgres',
    host: 'db',
    port: 5432,
    tls: { mode: 'prefer' },
    readOnly: false,
    options: {},
    ssh,
    savePassword: true,
    hasPassword: false,
    sortOrder: 0,
    createdAt: '',
    updatedAt: '',
  };
}

function Host({
  initial,
  profile,
  showErrors = false,
  onState,
}: {
  initial?: SshFormState;
  profile?: ConnectionProfile;
  showErrors?: boolean;
  onState?: (s: SshFormState) => void;
}) {
  const [state, setState] = useState(initial ?? initialSshForm(profile));
  onState?.(state);
  return (
    <SshSection
      value={state}
      onChange={(p) => setState((s) => ({ ...s, ...p }))}
      errors={showErrors ? validateSsh(state) : {}}
      profile={profile}
    />
  );
}

const renderHost = (props: Parameters<typeof Host>[0] = {}) =>
  render(
    <Providers>
      <Host {...props} />
    </Providers>,
  );

describe('SshSection', () => {
  it('only shows the toggle while the tunnel is off', () => {
    renderHost();
    expect(screen.getByRole('switch', { name: 'Connect through an SSH tunnel' })).not.toBeChecked();
    expect(screen.queryByLabelText('SSH host')).toBeNull();
  });

  it('reveals the password fields when enabled', async () => {
    const user = userEvent.setup();
    let last!: SshFormState;
    renderHost({ onState: (s) => (last = s) });
    await user.click(screen.getByRole('switch', { name: 'Connect through an SSH tunnel' }));
    await user.type(screen.getByLabelText('SSH host'), 'bastion');
    await user.clear(screen.getByLabelText('SSH port'));
    await user.type(screen.getByLabelText('SSH port'), '2200');
    await user.type(screen.getByLabelText('SSH user'), 'me');
    await user.type(screen.getByLabelText('SSH password'), 'pw');
    expect(last).toMatchObject({
      enabled: true,
      host: 'bastion',
      port: '2200',
      username: 'me',
      secret: 'pw',
      method: 'password',
    });
    expect(screen.queryByLabelText('Private key file')).toBeNull();
    expect(screen.getByLabelText('SSH password')).toHaveAttribute('type', 'password');
    await user.click(screen.getByRole('button', { name: 'Show secret' }));
    expect(screen.getByLabelText('SSH password')).toHaveAttribute('type', 'text');
  });

  it('switches between password, private key and agent', async () => {
    const user = userEvent.setup();
    renderHost({ initial: { ...initialSshForm(), enabled: true } });
    await user.click(screen.getByRole('combobox', { name: 'Authentication' }));
    await user.click(await screen.findByRole('option', { name: 'Private key' }));
    expect(screen.getByLabelText('Private key file')).toBeInTheDocument();
    expect(screen.queryByLabelText('SSH password')).toBeNull();
    expect(screen.queryByLabelText('Key passphrase')).toBeNull();
    await user.click(screen.getByRole('switch', { name: 'The key is protected by a passphrase' }));
    expect(screen.getByLabelText('Key passphrase')).toBeInTheDocument();

    await user.click(screen.getByRole('combobox', { name: 'Authentication' }));
    await user.click(await screen.findByRole('option', { name: 'SSH agent' }));
    expect(screen.queryByLabelText('Private key file')).toBeNull();
    expect(screen.queryByLabelText('Key passphrase')).toBeNull();
    expect(screen.getByText(/SSH_AUTH_SOCK/)).toBeInTheDocument();
  });

  it('browses for the key file', async () => {
    vi.mocked(pickFile).mockResolvedValue('/home/me/.ssh/id_ed25519');
    const user = userEvent.setup();
    renderHost({ initial: { ...initialSshForm(), enabled: true, method: 'privateKey' } });
    await user.click(screen.getByRole('button', { name: 'Browse for the private key file' }));
    expect(await screen.findByDisplayValue('/home/me/.ssh/id_ed25519')).toBeInTheDocument();
  });

  it('shows validation errors only when asked to', () => {
    const initial = { ...initialSshForm(), enabled: true };
    const { unmount } = renderHost({ initial });
    expect(screen.queryByRole('alert')).toBeNull();
    unmount();
    renderHost({ initial, showErrors: true });
    const alerts = screen.getAllByRole('alert').map((a) => a.textContent);
    expect(alerts).toContain('Enter the SSH host.');
    expect(alerts).toContain('Enter the SSH user.');
  });

  it('lets the user remove the stored secret of an edited profile', async () => {
    const user = userEvent.setup();
    let last!: SshFormState;
    const profile = profileWith({
      host: 'b',
      port: 22,
      username: 'u',
      auth: { method: 'password' },
    });
    renderHost({ profile, onState: (s) => (last = s) });
    expect(screen.getByLabelText('SSH password')).toHaveAttribute(
      'placeholder',
      'Stored in the keychain',
    );
    await user.click(screen.getByRole('button', { name: 'Remove stored secret' }));
    expect(last.secretAction).toBe('clear');
    expect(screen.getByLabelText('SSH password')).toHaveAttribute('placeholder', 'Will be removed');
    await user.click(screen.getByRole('button', { name: 'Keep stored secret' }));
    expect(last.secretAction).toBe('keep');
    // typing hides the remove button
    await user.type(screen.getByLabelText('SSH password'), 'x');
    expect(screen.queryByRole('button', { name: 'Remove stored secret' })).toBeNull();
  });

  it('is translated to Vietnamese', async () => {
    await i18n.changeLanguage('vi');
    renderHost({ initial: { ...initialSshForm(), enabled: true } });
    expect(screen.getByLabelText('Máy chủ SSH')).toBeInTheDocument();
    const section = screen.getByRole('region', { name: 'SSH tunnel' });
    expect(within(section).getByText('Mật khẩu SSH')).toBeInTheDocument();
    await i18n.changeLanguage('en');
  });
});
