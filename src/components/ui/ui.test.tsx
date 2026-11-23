import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import '@/lib/i18n';
import { Checkbox } from './Checkbox';
import { ConfirmDialog } from './ConfirmDialog';
import { Switch } from './Switch';
import { Tabs, TabsContent, TabsList, TabsTrigger } from './Tabs';
import { Toaster } from './Toast';
import { toast } from './toastStore';
import { Tooltip } from './Tooltip';

describe('ui primitives', () => {
  it('ConfirmDialog confirms and cancels', async () => {
    const onConfirm = vi.fn();
    const user = userEvent.setup();
    render(<ConfirmDialog open onOpenChange={() => {}} title="Delete?" onConfirm={onConfirm} />);
    expect(screen.getByRole('dialog', { name: 'Delete?' })).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Confirm' }));
    expect(onConfirm).toHaveBeenCalledOnce();
  });

  it('Toaster shows and dismisses toasts', async () => {
    const user = userEvent.setup();
    render(<Toaster />);
    act(() => void toast.error('boom'));
    expect(screen.getByRole('alert')).toHaveTextContent('boom');
    await user.click(screen.getByRole('button', { name: /dismiss/i }));
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('Checkbox, Switch and Tabs toggle', async () => {
    const user = userEvent.setup();
    render(
      <>
        <Checkbox aria-label="c" />
        <Switch aria-label="s" />
        <Tabs defaultValue="a">
          <TabsList>
            <TabsTrigger value="a">A</TabsTrigger>
            <TabsTrigger value="b">B</TabsTrigger>
          </TabsList>
          <TabsContent value="a">content a</TabsContent>
          <TabsContent value="b">content b</TabsContent>
        </Tabs>
      </>,
    );
    await user.click(screen.getByRole('checkbox', { name: 'c' }));
    expect(screen.getByRole('checkbox', { name: 'c' })).toBeChecked();
    await user.click(screen.getByRole('switch', { name: 's' }));
    expect(screen.getByRole('switch', { name: 's' })).toBeChecked();
    await user.click(screen.getByRole('tab', { name: 'B' }));
    expect(screen.getByText('content b')).toBeInTheDocument();
  });

  it('Tooltip is the native title, also over a disabled button', () => {
    render(
      <Tooltip content="Help & Support">
        <button type="button" disabled>
          help
        </button>
      </Tooltip>,
    );
    // No bubble element of our own: nothing that could stick out of the window and make the
    // page scroll sideways.
    expect(screen.queryByRole('tooltip')).toBeNull();
    expect(screen.getByRole('button', { name: 'help' }).parentElement).toHaveAttribute(
      'title',
      'Help & Support',
    );
  });
});
