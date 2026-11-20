import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import { StatusBar } from '@/app/StatusBar';
import i18n from '@/lib/i18n';
import { useAppearanceStore } from '@/stores/appearance';
import { ipc } from '@/lib/ipc';
import { resetMockSettings } from '@/lib/mock';
import { useSettingsDialog } from '@/features/workspace/paletteStore';
import { parseBounded, parseNumberField, ROWS_RANGE } from './queries';

beforeEach(async () => {
  resetMockSettings();
  useSettingsDialog.setState({ open: false });
  useAppearanceStore.getState().reset();
  await i18n.changeLanguage('en');
});

afterEach(() => {
  vi.restoreAllMocks();
});

function setup() {
  const user = userEvent.setup();
  render(
    <Providers>
      <StatusBar />
    </Providers>,
  );
  return user;
}

describe('parseBounded', () => {
  it('accepts grouped whole numbers in range only', () => {
    expect(parseBounded('1,000', ROWS_RANGE)).toBe(1000);
    expect(parseBounded(' 5 000 000 ', ROWS_RANGE)).toBe(5_000_000);
    expect(parseBounded('999', ROWS_RANGE)).toBeNull();
    expect(parseBounded('5000001', ROWS_RANGE)).toBeNull();
    expect(parseBounded('12a', ROWS_RANGE)).toBeNull();
    expect(parseBounded('', ROWS_RANGE)).toBeNull();
  });
});

describe('parseNumberField', () => {
  it('accepts 0 only for the fields where it means off', () => {
    expect(parseNumberField('keepaliveSecs', '0')).toBe(0);
    expect(parseNumberField('keepaliveSecs', '29')).toBeNull();
    expect(parseNumberField('keepaliveSecs', '30')).toBe(30);
    expect(parseNumberField('queryTimeoutSecs', '0')).toBe(0);
    expect(parseNumberField('queryTimeoutSecs', '86401')).toBeNull();
    expect(parseNumberField('editorMaxRows', '0')).toBeNull();
    expect(parseNumberField('editorMaxRows', '100')).toBe(100);
  });
});

describe('SettingsDialog', () => {
  it('shows the mock transport label on the status bar', () => {
    setup();
    expect(screen.getByTestId('ipc-transport')).toHaveTextContent('mock');
  });

  it('opens from the gear button and with Ctrl+, and saves values', async () => {
    const user = setup();
    await user.click(screen.getByRole('button', { name: 'Settings' }));
    let dialog = await screen.findByRole('dialog', { name: 'Settings' });
    expect(within(dialog).getByLabelText('Maximum rows loaded into a grid')).toHaveValue('1000000');
    expect(await within(dialog).findByTestId('log-dir')).toHaveTextContent('/mock/logs');
    const rows = within(dialog).getByLabelText('Maximum rows loaded into a grid');
    await user.clear(rows);
    await user.type(rows, '2000');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect((await ipc.settingsGet()).gridMaxRows).toBe(2000);

    await user.keyboard('{Control>},{/Control}');
    dialog = await screen.findByRole('dialog', { name: 'Settings' });
    expect(within(dialog).getByLabelText('Maximum rows loaded into a grid')).toHaveValue('2000');
  });

  it('saves the Query Advisor catalog switch', async () => {
    const user = setup();
    expect((await ipc.settingsGet()).planAdvisorMetadata).toBe(true);
    await user.click(screen.getByRole('button', { name: 'Settings' }));
    const dialog = await screen.findByRole('dialog', { name: 'Settings' });
    const toggle = within(dialog).getByRole('switch', {
      name: 'Match advice against existing indexes (reads the catalog)',
    });
    expect(toggle).toBeChecked();
    await user.click(toggle);
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect((await ipc.settingsGet()).planAdvisorMetadata).toBe(false);
  });

  it('keeps appearance changes as drafts until Save and discards Cancel', async () => {
    const user = setup();
    await user.click(screen.getByRole('button', { name: 'Settings' }));
    let dialog = await screen.findByRole('dialog', { name: 'Settings' });
    await user.click(within(dialog).getByLabelText('SQL editor'));
    await user.click(await screen.findByRole('option', { name: '18 px' }));
    expect(useAppearanceStore.getState().appearance.editorFontSize).toBe(14);
    await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await user.click(screen.getByRole('button', { name: 'Settings' }));
    dialog = await screen.findByRole('dialog', { name: 'Settings' });
    expect(within(dialog).getByLabelText('SQL editor')).toHaveTextContent('14 px');
    await user.click(within(dialog).getByLabelText('SQL editor'));
    await user.click(await screen.findByRole('option', { name: '18 px' }));
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(useAppearanceStore.getState().appearance.editorFontSize).toBe(18);
    expect(document.documentElement.style.getPropertyValue('--editor-font-size')).toBe('18px');
    expect(JSON.parse(localStorage.getItem('dbstudio.appearance')!).editorFontSize).toBe(18);
  });

  it('validates the ranges and keeps the dialog open', async () => {
    const user = setup();
    await user.click(screen.getByRole('button', { name: 'Settings' }));
    const dialog = await screen.findByRole('dialog', { name: 'Settings' });
    const rows = within(dialog).getByLabelText('Maximum rows loaded into a grid');
    const cells = within(dialog).getByLabelText('Maximum cells loaded into a grid');
    await user.clear(rows);
    await user.type(rows, '10');
    await user.clear(cells);
    await user.type(cells, '300000000');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    expect(await within(dialog).findByText(/Rows must be a whole number/)).toBeInTheDocument();
    expect(within(dialog).getByText(/Cells must be a whole number/)).toBeInTheDocument();
    expect((await ipc.settingsGet()).gridMaxRows).toBe(1_000_000);
  });

  it('shows a save error inside the dialog', async () => {
    vi.spyOn(ipc, 'settingsSet').mockRejectedValue({ kind: 'storage', message: 'read-only disk' });
    const user = setup();
    await user.click(screen.getByRole('button', { name: 'Settings' }));
    const dialog = await screen.findByRole('dialog', { name: 'Settings' });
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    expect(await within(dialog).findByText(/read-only disk/)).toBeInTheDocument();
  });

  it('saves the editor, connection and plan history settings', async () => {
    const user = setup();
    await user.click(screen.getByRole('button', { name: 'Settings' }));
    const dialog = await screen.findByRole('dialog', { name: 'Settings' });
    const type = async (label: string, value: string) => {
      const input = within(dialog).getByLabelText(label);
      await user.clear(input);
      await user.type(input, value);
    };
    await type('Rows kept per result set', '500');
    await type('Query timeout (seconds)', '30');
    await type('Keepalive interval (seconds)', '0');
    await user.click(
      within(dialog).getByRole('switch', { name: 'Record statements in the query history' }),
    );
    await user.click(
      within(dialog).getByRole('switch', {
        name: 'Reconnect automatically when a connection is lost',
      }),
    );
    await user.click(
      within(dialog).getByRole('switch', { name: 'Record explained plans in the plan history' }),
    );
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(await ipc.settingsGet()).toMatchObject({
      editorMaxRows: 500,
      queryTimeoutMs: 30_000,
      historyEnabled: false,
      autoReconnect: false,
      keepaliveSecs: 0,
      planHistoryEnabled: false,
      language: 'en',
    });
  }, 15_000);

  it('rejects a keepalive below the minimum and keeps the dialog open', async () => {
    const user = setup();
    await user.click(screen.getByRole('button', { name: 'Settings' }));
    const dialog = await screen.findByRole('dialog', { name: 'Settings' });
    const keepalive = within(dialog).getByLabelText('Keepalive interval (seconds)');
    await user.clear(keepalive);
    await user.type(keepalive, '5');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    expect(
      await within(dialog).findByText('Enter 0 or a whole number from 30 to 3600'),
    ).toBeInTheDocument();
    expect((await ipc.settingsGet()).keepaliveSecs).toBe(240);
  });

  it('labels a value the backend rejects', async () => {
    vi.spyOn(ipc, 'settingsSet').mockRejectedValue({
      kind: 'invalid',
      message: 'keepaliveSecs is out of range',
    });
    const user = setup();
    await user.click(screen.getByRole('button', { name: 'Settings' }));
    const dialog = await screen.findByRole('dialog', { name: 'Settings' });
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    expect(
      await within(dialog).findByText('Invalid value: keepaliveSecs is out of range'),
    ).toBeInTheDocument();
  });

  it('saves the background settings at once and keeps them when the dialog is saved', async () => {
    const set = vi.spyOn(ipc, 'backgroundSet');
    const user = setup();
    await user.click(screen.getByRole('button', { name: 'Settings' }));
    const dialog = await screen.findByRole('dialog', { name: 'Settings' });
    await user.click(
      await within(dialog).findByRole('switch', { name: 'Start DBStudio when I sign in' }),
    );
    await waitFor(() => expect(set).toHaveBeenCalledWith({ closeToTray: 'auto', autostart: true }));
    await user.click(within(dialog).getByRole('combobox', { name: 'When the window is closed' }));
    await user.click(
      await screen.findByRole('option', { name: 'Always keep running in the tray' }),
    );
    await waitFor(() =>
      expect(set).toHaveBeenLastCalledWith({ closeToTray: 'always', autostart: true }),
    );
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect((await ipc.backgroundGet()).closeToTray).toBe('always');
  });
});
