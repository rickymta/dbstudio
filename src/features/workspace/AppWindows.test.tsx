import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { Providers } from '@/app/providers';
import { StatusBar } from '@/app/StatusBar';
import { ShortcutsDialog } from '@/features/editor/ShortcutsDialog';
import { matchesShortcut } from '@/features/editor/shortcuts';
import { useShortcutsHelp } from '@/features/editor/shortcutsHelpStore';
import i18n from '@/lib/i18n';
import { mockSupportOutbox } from '@/lib/mock';
import { mockEmit } from '@/lib/mock/events';
import { resetActivity, useActivity } from '@/stores/activity';
import { useTabsStore, WELCOME_TAB } from '@/stores/tabs';
import { AppWindows } from './AppWindows';
import { useActivityWindow, useHelpDialog } from './paletteStore';
import { useZoom } from './zoom';

function setup() {
  const user = userEvent.setup();
  render(
    <Providers>
      <StatusBar />
      <AppWindows />
      <ShortcutsDialog />
    </Providers>,
  );
  return user;
}

beforeEach(async () => {
  await i18n.changeLanguage('en');
  resetActivity();
  mockSupportOutbox.length = 0;
  localStorage.clear();
  useActivityWindow.setState({ open: false });
  useHelpDialog.setState({ open: false });
  useShortcutsHelp.setState({ open: false });
  useZoom.getState().setZoom(1);
  useTabsStore.setState({ tabs: [WELCOME_TAB], activeId: WELCOME_TAB.id });
});

const key = (k: string, init: KeyboardEventInit = {}) =>
  new KeyboardEvent('keydown', { key: k, bubbles: true, cancelable: true, ...init });

describe('matchesShortcut', () => {
  it('accepts the alternatives and Shift for symbols', () => {
    expect(matchesShortcut(key('=', { ctrlKey: true }), 'zoomIn', false)).toBe(true);
    expect(matchesShortcut(key('+', { ctrlKey: true, shiftKey: true }), 'zoomIn', false)).toBe(
      true,
    );
    expect(matchesShortcut(key('-', { ctrlKey: true }), 'zoomOut', false)).toBe(true);
    expect(matchesShortcut(key('0', { ctrlKey: true }), 'zoomReset', false)).toBe(true);
    expect(matchesShortcut(key('L', { ctrlKey: true, shiftKey: true }), 'activity', false)).toBe(
      true,
    );
    expect(matchesShortcut(key('l', { ctrlKey: true }), 'activity', false)).toBe(false);
    expect(matchesShortcut(key('=', {}), 'zoomIn', false)).toBe(false);
  });
});

describe('zoom', () => {
  it('changes from the status bar and the keyboard, within limits', async () => {
    const user = setup();
    const control = screen.getByTestId('zoom-control');
    expect(control).toHaveTextContent('100%');
    await user.click(within(control).getByRole('button', { name: 'Zoom in' }));
    expect(control).toHaveTextContent('110%');
    await user.keyboard('{Control>}-{/Control}');
    await user.keyboard('{Control>}-{/Control}');
    expect(control).toHaveTextContent('90%');
    expect(localStorage.getItem('dbstudio.zoom')).toBe('0.9');
    await user.keyboard('{Control>}0{/Control}');
    expect(control).toHaveTextContent('100%');
    act(() => useZoom.getState().setZoom(9));
    expect(useZoom.getState().zoom).toBe(2);
    act(() => useZoom.getState().setZoom(0));
    expect(useZoom.getState().zoom).toBe(0.6);
  });
});

describe('Activity Logs', () => {
  it('opens from the status bar, lists the application entries and clears them', async () => {
    const user = setup();
    act(() => {
      useActivity
        .getState()
        .log({ connectionId: null, event: 'saveSettings', status: 'finished', ms: 12 });
      useActivity.getState().log({
        connectionId: null,
        event: 'importConnections',
        status: 'failed',
        error: 'bad file',
      });
    });
    await user.click(screen.getByRole('button', { name: 'Activity Logs' }));
    const dialog = await screen.findByRole('dialog', { name: 'Activity Logs' });
    const log = within(dialog).getByTestId('activity-log');
    expect(log).toHaveTextContent('Save settings: finished');
    expect(log).toHaveTextContent('12 ms');
    expect(log).toHaveTextContent('Import connections: failed');
    expect(log).toHaveTextContent('bad file');
    expect(within(dialog).getByRole('tab', { name: /Connection/ })).toBeDisabled();
    await user.click(within(dialog).getByRole('button', { name: 'Clear log' }));
    expect(log).toHaveTextContent('Nothing logged yet.');
  });

  it('toggles with Ctrl+Shift+L and records connection health events', async () => {
    const user = setup();
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    act(() =>
      mockEmit('session://state', {
        sessionId: 's1',
        connectionId: 'c1',
        state: 'gaveUp',
        attempt: 5,
        error: { kind: 'connection', message: 'refused' },
        leasesBroken: [],
        leasesReopened: [],
      }),
    );
    expect(useActivity.getState().items.at(-1)).toMatchObject({
      connectionId: 'c1',
      event: 'reconnectGaveUp',
      status: 'failed',
      error: 'refused',
    });
    await user.keyboard('{Control>}{Shift>}L{/Shift}{/Control}');
    expect(await screen.findByRole('dialog', { name: 'Activity Logs' })).toBeInTheDocument();
    await user.keyboard('{Control>}{Shift>}L{/Shift}{/Control}');
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  });
});

describe('Keyboard shortcuts window', () => {
  it('groups the shortcuts, includes the debugger keymap and filters', async () => {
    const user = setup();
    await user.click(screen.getByRole('button', { name: 'Keyboard shortcuts' }));
    const dialog = await screen.findByRole('dialog', { name: 'Keyboard shortcuts' });
    for (const group of ['Query editor', 'Application', 'Window', 'Debugger (JetBrains keymap)']) {
      expect(within(dialog).getByRole('region', { name: group })).toBeInTheDocument();
    }
    const debuggerGroup = within(dialog).getByRole('region', { name: /Debugger/ });
    expect(debuggerGroup).toHaveTextContent('Step Over');
    expect(debuggerGroup).toHaveTextContent('F8');
    await user.type(within(dialog).getByRole('textbox', { name: 'Filter shortcuts' }), 'zoom');
    expect(within(dialog).getAllByTestId('shortcut-row')).toHaveLength(3);
    await user.clear(within(dialog).getByRole('textbox', { name: 'Filter shortcuts' }));
    await user.type(within(dialog).getByRole('textbox', { name: 'Filter shortcuts' }), 'qqq');
    expect(within(dialog).getByText('No shortcut matches.')).toBeInTheDocument();
  });
});

describe('Help & Support', () => {
  it('shows the version, validates the form and sends it', async () => {
    const user = setup();
    await user.click(screen.getByRole('button', { name: 'Help & Support' }));
    const dialog = await screen.findByRole('dialog', { name: 'Help & Support' });
    await waitFor(() =>
      expect(within(dialog).getByTestId('help-version')).toHaveTextContent(/DBStudio \d/),
    );

    await user.click(within(dialog).getByRole('button', { name: 'Submit' }));
    expect(within(dialog).getAllByText('This field is required.')).toHaveLength(4);
    expect(within(dialog).getByText('Choose a topic.')).toBeInTheDocument();
    expect(mockSupportOutbox).toHaveLength(0);

    await user.type(within(dialog).getByLabelText('Name'), 'An');
    await user.type(within(dialog).getByLabelText('Email'), 'an@example');
    await user.click(within(dialog).getByRole('combobox', { name: 'What do you need help with?' }));
    await user.click(await screen.findByRole('option', { name: 'Bug report' }));
    await user.type(within(dialog).getByLabelText('Subject'), 'Crash');
    await user.type(within(dialog).getByLabelText('Message'), 'It crashed.');
    await user.click(within(dialog).getByRole('button', { name: 'Submit' }));
    expect(within(dialog).getByText('Enter a valid email address.')).toBeInTheDocument();

    await user.type(within(dialog).getByLabelText('Email'), '.com');
    await user.click(within(dialog).getByRole('button', { name: 'Submit' }));
    expect(await within(dialog).findByText('Message sent')).toBeInTheDocument();
    expect(mockSupportOutbox).toEqual([
      {
        name: 'An',
        email: 'an@example.com',
        topic: 'bug',
        subject: 'Crash',
        message: 'It crashed.',
        includeSystem: true,
      },
    ]);
    // Name and email are remembered for the next request.
    await user.click(within(dialog).getByRole('button', { name: 'Send another' }));
    expect(within(dialog).getByLabelText('Name')).toHaveValue('An');
    expect(within(dialog).getByLabelText('Subject')).toHaveValue('');
  }, 20_000);
});
