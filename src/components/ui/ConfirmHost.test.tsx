import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeAll, describe, expect, it } from 'vitest';
import i18n from '@/lib/i18n';
import { confirmDialog, useConfirmStore } from './confirm';
import { ConfirmHost } from './ConfirmHost';

beforeAll(() => i18n.changeLanguage('en'));

describe('confirmDialog', () => {
  it('resolves true only from the confirm button', async () => {
    render(<ConfirmHost />);
    let answer: Promise<boolean>;
    act(() => {
      answer = confirmDialog({
        title: 'Unsaved changes',
        description: 'Discard them?',
        confirmLabel: 'Discard',
        destructive: true,
      });
    });
    const dialog = await screen.findByRole('dialog', { name: 'Unsaved changes' });
    expect(dialog).toHaveTextContent('Discard them?');
    await userEvent.click(screen.getByRole('button', { name: 'Discard' }));
    await expect(answer!).resolves.toBe(true);
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(useConfirmStore.getState().queue).toEqual([]);
  });

  it('resolves false on cancel and on Escape, and shows queued questions in turn', async () => {
    render(<ConfirmHost />);
    let first: Promise<boolean>;
    let second: Promise<boolean>;
    act(() => {
      first = confirmDialog({ title: 'First' });
      second = confirmDialog({ title: 'Second' });
    });
    await screen.findByRole('dialog', { name: 'First' });
    await userEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    await expect(first!).resolves.toBe(false);
    await screen.findByRole('dialog', { name: 'Second' });
    await userEvent.keyboard('{Escape}');
    await expect(second!).resolves.toBe(false);
    expect(screen.queryByRole('dialog')).toBeNull();
  });
});
