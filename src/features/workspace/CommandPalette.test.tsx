import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import { EditorHost } from '@/features/editor/EditorHost';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { useSessionsStore } from '@/stores/sessions';
import { WELCOME_TAB, useTabsStore } from '@/stores/tabs';
import { useThemeStore } from '@/stores/theme';
import { CommandPalette } from './CommandPalette';
import {
  filterCommands,
  useCommandPalette,
  useSettingsDialog,
  type PaletteCommand,
} from './paletteStore';
import { useNavigation } from './navigation';

const command = (label: string, detail?: string): PaletteCommand => ({
  id: label,
  group: 'action',
  label,
  detail,
  run: () => undefined,
});

describe('filterCommands', () => {
  const list = [
    command('Toggle sidebar'),
    command('Settings'),
    command('orders', 'PostgreSQL dev › app_dev'),
    command('Kết nối mới…'),
    command('New query'),
  ];

  it('returns everything for an empty query', () => {
    expect(filterCommands(list, '  ')).toBe(list);
  });

  it('matches every word in the label or the detail, ignoring case and accents', () => {
    expect(filterCommands(list, 'ket noi').map((c) => c.label)).toEqual(['Kết nối mới…']);
    expect(filterCommands(list, 'postgres ord').map((c) => c.label)).toEqual(['orders']);
    expect(filterCommands(list, 'zzz')).toEqual([]);
  });

  it('puts labels that start with the query first', () => {
    expect(filterCommands(list, 's').map((c) => c.label)).toEqual([
      'Settings',
      'Toggle sidebar',
      'orders',
    ]);
  });
});

async function openPalette() {
  const user = userEvent.setup();
  render(
    <Providers>
      <CommandPalette />
    </Providers>,
  );
  act(() => useCommandPalette.getState().setOpen(true));
  const input = await screen.findByRole('combobox', { name: 'Command palette' });
  return { user, input };
}

beforeEach(async () => {
  await i18n.changeLanguage('en');
  useCommandPalette.setState({ open: false });
  useSettingsDialog.setState({ open: false });
  useThemeStore.getState().setMode('system');
  useNavigation.setState({ recent: [], favorites: [], request: null });
  useTabsStore.setState({ tabs: [WELCOME_TAB], activeId: WELCOME_TAB.id });
  useSessionsStore.setState({ sessionsByConnection: {}, opening: {} });
});

afterEach(() => vi.restoreAllMocks());

describe('CommandPalette', () => {
  it('lists the commands and the mock connections', async () => {
    await openPalette();
    const list = screen.getByRole('listbox');
    expect(within(list).getByRole('option', { name: /^Settings/ })).toBeInTheDocument();
    expect(await within(list).findByRole('option', { name: /PostgreSQL dev/ })).toBeInTheDocument();
  });

  it('runs the selected command with Enter and closes', async () => {
    const { user, input } = await openPalette();
    await user.type(input, 'settings');
    expect(screen.getAllByRole('option')).toHaveLength(1);
    await user.keyboard('{Enter}');
    expect(useSettingsDialog.getState().open).toBe(true);
    await waitFor(() => expect(screen.queryByRole('listbox')).toBeNull());
  });

  it('moves with the arrow keys and wraps around', async () => {
    const { user, input } = await openPalette();
    await user.type(input, 'theme');
    const options = screen.getAllByRole('option');
    expect(options.map((o) => o.textContent)).toEqual(['Theme: Light', 'Theme: Dark']);
    await user.keyboard('{ArrowDown}');
    expect(options[1]).toHaveAttribute('aria-selected', 'true');
    await user.keyboard('{ArrowDown}');
    expect(options[0]).toHaveAttribute('aria-selected', 'true');
    await user.keyboard('{ArrowUp}{Enter}');
    expect(useThemeStore.getState().mode).toBe('dark');
  });

  it('asks the sidebar to open a connection', async () => {
    const { user, input } = await openPalette();
    await user.type(input, 'mysql staging');
    await user.click(await screen.findByRole('option', { name: /MySQL staging/ }));
    expect(useNavigation.getState().request?.connectionId).toBeTruthy();
  });

  it('says so when nothing matches', async () => {
    const { user, input } = await openPalette();
    await user.type(input, 'qqqq');
    expect(screen.getByText('Nothing matches.')).toBeInTheDocument();
  });
});

const PG_DEV = '00000000-0000-4000-8000-0000000000b1';
const WAIT = { timeout: 4000 };

describe('Spotlight: objects of the open connections', () => {
  it('finds a table by name on the server and opens its data', async () => {
    const spy = vi.spyOn(ipc, 'objectSearch');
    await useSessionsStore.getState().open(PG_DEV);
    const { user, input } = await openPalette();
    await user.type(input, 'orders');
    // Never expanded in the tree, never opened before: only the server knows it.
    const row = await screen.findByRole('option', { name: /^orders\s*PostgreSQL dev/ }, WAIT);
    expect(screen.getByText('Objects on the server')).toBeInTheDocument();
    expect(spy.mock.calls.at(-1)![1]).toMatchObject({ query: 'orders' });
    await user.click(row);
    const tab = useTabsStore.getState().tabs.find((t) => t.title === 'orders');
    expect(tab).toMatchObject({ kind: 'table' });
    expect(useTabsStore.getState().activeId).toBe(tab!.id);
  });

  it('finds a function by name and opens its DDL', async () => {
    await useSessionsStore.getState().open(PG_DEV);
    const { user, input } = await openPalette();
    await user.type(input, 'SLUG');
    await user.click(await screen.findByRole('option', { name: /^slugify/ }, WAIT));
    expect(useTabsStore.getState().tabs.find((t) => t.title === 'slugify')).toMatchObject({
      kind: 'ddl',
    });
  });

  it('does not ask the server without an open connection or for a single character', async () => {
    const spy = vi.spyOn(ipc, 'objectSearch');
    const { user, input } = await openPalette();
    await user.type(input, 'orders');
    await new Promise((r) => setTimeout(r, 400));
    expect(spy).not.toHaveBeenCalled();
    expect(screen.getByText('Nothing matches.')).toBeInTheDocument();
  });

  it('opens with Ctrl+F instead of the find bar of the webview', async () => {
    const user = userEvent.setup();
    render(
      <Providers>
        <EditorHost />
        <CommandPalette />
      </Providers>,
    );
    const event = new KeyboardEvent('keydown', {
      key: 'f',
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    });
    act(() => void document.body.dispatchEvent(event));
    expect(event.defaultPrevented).toBe(true);
    const input = await screen.findByRole('combobox', { name: 'Command palette' });
    expect(input).toHaveFocus();

    // With a dialog open the keys do nothing, but still never reach the webview.
    const again = new KeyboardEvent('keydown', {
      key: 'f',
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    });
    act(() => void document.body.dispatchEvent(again));
    expect(again.defaultPrevented).toBe(true);
    await user.keyboard('{Escape}');
    await waitFor(() => expect(screen.queryByRole('listbox')).toBeNull());
  });
});
