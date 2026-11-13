import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { I18nextProvider } from 'react-i18next';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import i18n from '@/lib/i18n';
import type { ConfirmationRequest } from '@/lib/types';
import { useConfirmStore } from './confirmStore';
import { ConfirmDestructiveDialog } from './ConfirmDestructiveDialog';
import { ConfirmDestructiveHost } from './ConfirmDestructiveHost';
import { guardedCall } from './useGuardedCall';

const request: ConfirmationRequest = {
  token: 'a'.repeat(32),
  confirmText: 'app_prod',
  expiresInMs: 60_000,
  statements: [
    {
      index: 0,
      range: { start: 0, end: 12 },
      text: 'DROP TABLE t',
      reason: 'drop',
    },
    {
      index: 1,
      range: { start: 14, end: 30 },
      text: 'DELETE FROM users',
      reason: 'deleteWithoutWhere',
    },
  ],
};

beforeEach(async () => {
  await i18n.changeLanguage('en');
  useConfirmStore.setState({ queue: [] });
});

const wrap = (node: React.ReactNode) => <I18nextProvider i18n={i18n}>{node}</I18nextProvider>;

describe('ConfirmDestructiveDialog', () => {
  it('lists the statements and enables the button only for the exact database name', async () => {
    const user = userEvent.setup();
    const onConfirm = vi.fn();
    render(wrap(<ConfirmDestructiveDialog request={request} onConfirm={onConfirm} onCancel={vi.fn()} />));
    expect(screen.getByText('DROP TABLE t')).toBeInTheDocument();
    expect(screen.getByText('DELETE FROM users')).toBeInTheDocument();
    const confirm = screen.getByRole('button', { name: 'Run anyway' });
    const input = screen.getByLabelText(/app_prod/);
    expect(confirm).toBeDisabled();

    for (const wrong of ['app', 'APP_PROD', 'app_prod ', 'app_prod2']) {
      await user.clear(input);
      await user.type(input, wrong);
      expect(confirm).toBeDisabled();
    }
    await user.clear(input);
    await user.type(input, 'app_prod');
    expect(confirm).toBeEnabled();
    await user.click(confirm);
    expect(onConfirm).toHaveBeenCalledWith({ token: request.token, typed: 'app_prod' });
  });

  it('ignores Enter until the text matches and cancels on request', async () => {
    const user = userEvent.setup();
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    render(wrap(<ConfirmDestructiveDialog request={request} onConfirm={onConfirm} onCancel={onCancel} />));
    const input = screen.getByLabelText(/app_prod/);
    await user.type(input, 'app{Enter}');
    expect(onConfirm).not.toHaveBeenCalled();
    await user.type(input, '_prod{Enter}');
    expect(onConfirm).toHaveBeenCalledTimes(1);
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(onCancel).toHaveBeenCalled();
  });

  it('renders nothing without a request', () => {
    const { container } = render(
      wrap(<ConfirmDestructiveDialog request={null} onConfirm={vi.fn()} onCancel={vi.fn()} />),
    );
    expect(container).toBeEmptyDOMElement();
  });
});

describe('guardedCall', () => {
  it('asks, then calls again with the grant', async () => {
    const call = vi
      .fn()
      .mockRejectedValueOnce({ kind: 'confirmationRequired', message: 'm', confirmation: request })
      .mockResolvedValueOnce('done');
    const ask = vi.fn().mockResolvedValue({ token: request.token, typed: 'app_prod' });
    await expect(guardedCall(call, ask)).resolves.toBe('done');
    expect(ask).toHaveBeenCalledWith(request);
    expect(call).toHaveBeenNthCalledWith(1, undefined);
    expect(call).toHaveBeenNthCalledWith(2, { token: request.token, typed: 'app_prod' });
  });

  it('rethrows other errors and the user declining', async () => {
    const boom = { kind: 'query', message: 'boom' };
    await expect(guardedCall(() => Promise.reject(boom), vi.fn())).rejects.toMatchObject(boom);
    const declined = { kind: 'cancelled', message: 'no' };
    const call = () =>
      Promise.reject({ kind: 'confirmationRequired', message: 'm', confirmation: request });
    await expect(guardedCall(call, () => Promise.reject(declined))).rejects.toMatchObject(declined);
  });

  it('gives up when the backend keeps asking', async () => {
    const call = vi.fn(() =>
      Promise.reject({ kind: 'confirmationRequired', message: 'm', confirmation: request }),
    );
    const ask = vi.fn().mockResolvedValue({ token: 't', typed: 'x' });
    await expect(guardedCall(call, ask)).rejects.toMatchObject({ kind: 'confirmationRequired' });
    expect(ask).toHaveBeenCalledTimes(3);
  });
});

describe('ConfirmDestructiveHost', () => {
  it('shows the queued request and resolves the pending call with the grant', async () => {
    const user = userEvent.setup();
    render(wrap(<ConfirmDestructiveHost />));
    const call = vi
      .fn()
      .mockRejectedValueOnce({ kind: 'confirmationRequired', message: 'm', confirmation: request })
      .mockResolvedValueOnce(7);
    let result: Promise<number> | undefined;
    act(() => {
      result = guardedCall(call);
    });
    await user.type(await screen.findByLabelText(/app_prod/), 'app_prod');
    await user.click(screen.getByRole('button', { name: 'Run anyway' }));
    await expect(result).resolves.toBe(7);
    expect(call).toHaveBeenLastCalledWith({ token: request.token, typed: 'app_prod' });
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  });

  it('rejects with cancelled when the user closes the dialog', async () => {
    const user = userEvent.setup();
    render(wrap(<ConfirmDestructiveHost />));
    let result: Promise<unknown> | undefined;
    act(() => {
      result = guardedCall(() =>
        Promise.reject({ kind: 'confirmationRequired', message: 'm', confirmation: request }),
      );
    });
    result?.catch(() => undefined);
    await user.click(await screen.findByRole('button', { name: 'Cancel' }));
    await expect(result).rejects.toMatchObject({ kind: 'cancelled' });
  });
});
