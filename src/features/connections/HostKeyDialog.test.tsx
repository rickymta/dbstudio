import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import type { IpcError } from '@/lib/types';
import { HostKeyDialog } from './HostKeyDialog';
import { hostKeyPromptFromError } from './hostKeyPrompt';
import { useHostKeyPrompt } from './useHostKeyPrompt';

beforeEach(async () => {
  vi.restoreAllMocks();
  await i18n.changeLanguage('en');
});

const KEY: IpcError = {
  kind: 'sshHostKey',
  message: 'bastion',
  host: 'bastion',
  port: 2222,
  keyType: 'ssh-ed25519',
  fingerprint: 'SHA256:abc123',
  changed: false,
};

describe('hostKeyPromptFromError', () => {
  it('extracts the prompt from an sshHostKey error only', () => {
    expect(hostKeyPromptFromError(KEY)).toEqual({
      host: 'bastion',
      port: 2222,
      keyType: 'ssh-ed25519',
      fingerprint: 'SHA256:abc123',
      changed: false,
    });
    expect(hostKeyPromptFromError({ ...KEY, changed: true })?.changed).toBe(true);
    expect(hostKeyPromptFromError({ ...KEY, port: undefined })?.port).toBe(22);
    expect(hostKeyPromptFromError({ kind: 'connection', message: 'x' })).toBeNull();
    expect(hostKeyPromptFromError({ ...KEY, fingerprint: undefined })).toBeNull();
  });
});

describe('HostKeyDialog', () => {
  it('shows the server and fingerprint and trusts the key', async () => {
    const trust = vi.spyOn(ipc, 'sshTrustHost').mockResolvedValue();
    const onTrusted = vi.fn();
    const onCancel = vi.fn();
    const user = userEvent.setup();
    render(
      <Providers>
        <HostKeyDialog
          prompt={hostKeyPromptFromError(KEY)}
          onCancel={onCancel}
          onTrusted={onTrusted}
        />
      </Providers>,
    );
    const dialog = await screen.findByRole('dialog', { name: 'Trust this SSH server?' });
    expect(dialog).toHaveTextContent('bastion:2222');
    expect(screen.getByTestId('host-key-fingerprint')).toHaveTextContent('SHA256:abc123');
    expect(dialog).toHaveTextContent('ssh-ed25519');
    expect(screen.queryByRole('note')).toBeNull();
    await user.click(screen.getByRole('button', { name: 'Trust and continue' }));
    await waitFor(() => expect(onTrusted).toHaveBeenCalledTimes(1));
    expect(trust).toHaveBeenCalledWith('bastion', 2222, 'ssh-ed25519', 'SHA256:abc123');
    expect(onCancel).not.toHaveBeenCalled();
  });

  it('warns loudly when the key has changed', async () => {
    render(
      <Providers>
        <HostKeyDialog
          prompt={hostKeyPromptFromError({ ...KEY, changed: true })}
          onCancel={() => {}}
          onTrusted={() => {}}
        />
      </Providers>,
    );
    await screen.findByRole('dialog', { name: 'SSH host key has changed' });
    expect(screen.getByRole('note')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Replace the trusted key' })).toBeInTheDocument();
  });

  it('cancel does not store anything', async () => {
    const trust = vi.spyOn(ipc, 'sshTrustHost').mockResolvedValue();
    const onCancel = vi.fn();
    const user = userEvent.setup();
    render(
      <Providers>
        <HostKeyDialog
          prompt={hostKeyPromptFromError(KEY)}
          onCancel={onCancel}
          onTrusted={() => {}}
        />
      </Providers>,
    );
    await user.click(await screen.findByRole('button', { name: 'Cancel' }));
    expect(onCancel).toHaveBeenCalled();
    expect(trust).not.toHaveBeenCalled();
  });

  it('keeps the dialog open and shows the error when storing fails', async () => {
    vi.spyOn(ipc, 'sshTrustHost').mockRejectedValue({ kind: 'storage', message: 'disk full' });
    const onTrusted = vi.fn();
    const user = userEvent.setup();
    render(
      <Providers>
        <HostKeyDialog
          prompt={hostKeyPromptFromError(KEY)}
          onCancel={() => {}}
          onTrusted={onTrusted}
        />
      </Providers>,
    );
    await user.click(await screen.findByRole('button', { name: 'Trust and continue' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('disk full');
    expect(onTrusted).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: 'Trust and continue' })).toBeEnabled();
  });

  it('is closed without a prompt', () => {
    render(
      <Providers>
        <HostKeyDialog prompt={null} onCancel={() => {}} onTrusted={() => {}} />
      </Providers>,
    );
    expect(screen.queryByRole('dialog')).toBeNull();
  });
});

function Harness({ op }: { op: () => Promise<string> }) {
  const { run, dialog } = useHostKeyPrompt();
  const [out, setOut] = useState('idle');
  return (
    <div>
      <button
        onClick={() =>
          void run(op).then(
            (v) => setOut(`ok:${v}`),
            (e: IpcError) => setOut(`err:${e.kind}`),
          )
        }
      >
        go
      </button>
      <p data-testid="out">{out}</p>
      {dialog}
    </div>
  );
}

describe('useHostKeyPrompt', () => {
  it('retries the operation after the key was trusted', async () => {
    vi.spyOn(ipc, 'sshTrustHost').mockResolvedValue();
    const op = vi
      .fn<() => Promise<string>>()
      .mockRejectedValueOnce(KEY)
      .mockResolvedValueOnce('connected');
    const user = userEvent.setup();
    render(
      <Providers>
        <Harness op={op} />
      </Providers>,
    );
    await user.click(screen.getByRole('button', { name: 'go' }));
    await user.click(await screen.findByRole('button', { name: 'Trust and continue' }));
    await waitFor(() => expect(screen.getByTestId('out')).toHaveTextContent('ok:connected'));
    expect(op).toHaveBeenCalledTimes(2);
  });

  it('fails with the original error when the user cancels', async () => {
    const trust = vi.spyOn(ipc, 'sshTrustHost').mockResolvedValue();
    const op = vi.fn<() => Promise<string>>().mockRejectedValue(KEY);
    const user = userEvent.setup();
    render(
      <Providers>
        <Harness op={op} />
      </Providers>,
    );
    await user.click(screen.getByRole('button', { name: 'go' }));
    await user.click(await screen.findByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.getByTestId('out')).toHaveTextContent('err:sshHostKey'));
    expect(op).toHaveBeenCalledTimes(1);
    expect(trust).not.toHaveBeenCalled();
  });

  it('does not intercept other errors', async () => {
    const op = vi
      .fn<() => Promise<string>>()
      .mockRejectedValue({ kind: 'authentication', message: 'no' });
    const user = userEvent.setup();
    render(
      <Providers>
        <Harness op={op} />
      </Providers>,
    );
    await user.click(screen.getByRole('button', { name: 'go' }));
    await waitFor(() => expect(screen.getByTestId('out')).toHaveTextContent('err:authentication'));
    expect(screen.queryByRole('dialog')).toBeNull();
  });
});
