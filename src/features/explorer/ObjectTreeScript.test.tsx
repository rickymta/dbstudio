import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useEditorStore } from '@/features/editor/editorStore';
import { openSession, resetEditorWorld } from '@/features/editor/testUtils';
import {
  openRedisSession,
  renderWithProviders as renderKv,
  resetRedisMocks,
} from '@/features/keyvalue/testUtils';
import { useSessionsStore } from '@/stores/sessions';
import { useTabsStore } from '@/stores/tabs';
import { ObjectTree } from './ObjectTree';

const WAIT = { timeout: 4000 };
const item = (name: string | RegExp) => screen.findByRole('treeitem', { name }, WAIT);

async function expand(name: string | RegExp) {
  const row = await item(name);
  fireEvent.click(within(row).getByRole('button', { name: 'Expand' }));
}

async function openPgTree() {
  const info = await openSession();
  renderKv(<ObjectTree connectionId={info.connectionId} />);
  await expand('app_dev');
  await expand('Schemas');
  await expand('public');
  await expand('Tables');
  await expand('Views');
}

describe('"Script as" menu of the object tree', () => {
  beforeEach(() => resetEditorWorld());

  it('offers the four templates on a table and opens a new editor that is not run', async () => {
    const user = userEvent.setup();
    await openPgTree();
    fireEvent.contextMenu(await item(/^users/));
    await user.click(await screen.findByRole('menuitem', { name: 'Script as' }));
    const names = (await screen.findAllByRole('menuitem'))
      .map((e) => e.textContent)
      .filter((n) => /^(SELECT|INSERT|UPDATE|DELETE)$/.test(n ?? ''));
    expect(names).toEqual(['SELECT', 'INSERT', 'UPDATE', 'DELETE']);
    fireEvent.click(screen.getByRole('menuitem', { name: 'DELETE' }));
    await waitFor(() => {
      const tab = useTabsStore.getState().tabs.find((t) => t.kind === 'editor');
      expect(tab?.title).toBe('DELETE users');
    }, WAIT);
    const data = Object.values(useEditorStore.getState().tabs);
    expect(data).toHaveLength(1);
    expect(data[0]?.text).toContain('DELETE FROM');
    expect(data[0]?.schema).toBe('public');
    expect(data[0]?.database).toBe('app_dev');
  });

  it('opens a second script with a distinct title', async () => {
    const user = userEvent.setup();
    await openPgTree();
    for (let i = 0; i < 2; i++) {
      fireEvent.contextMenu(await item(/^users/));
      await user.click(await screen.findByRole('menuitem', { name: 'Script as' }));
      fireEvent.click(await screen.findByRole('menuitem', { name: 'SELECT' }));
      await waitFor(
        () =>
          expect(useTabsStore.getState().tabs.filter((t) => t.kind === 'editor')).toHaveLength(
            i + 1,
          ),
        WAIT,
      );
    }
    const titles = useTabsStore
      .getState()
      .tabs.filter((t) => t.kind === 'editor')
      .map((t) => t.title);
    expect(titles).toEqual(['SELECT users', 'SELECT users (2)']);
  });

  it('copies the script to the clipboard without opening an editor', async () => {
    const user = userEvent.setup();
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    await openPgTree();
    fireEvent.contextMenu(await item(/^users/));
    await user.click(await screen.findByRole('menuitem', { name: 'Copy script as' }));
    fireEvent.click(await screen.findByRole('menuitem', { name: 'UPDATE' }));
    await waitFor(
      () => expect(writeText).toHaveBeenCalledWith(expect.stringContaining('UPDATE')),
      WAIT,
    );
    expect(useTabsStore.getState().tabs.some((t) => t.kind === 'editor')).toBe(false);
  });

  it('offers SELECT only on a view', async () => {
    const user = userEvent.setup();
    await openPgTree();
    fireEvent.contextMenu(await item(/^active_users/));
    await user.click(await screen.findByRole('menuitem', { name: 'Script as' }));
    expect(await screen.findByRole('menuitem', { name: 'SELECT' })).toBeInTheDocument();
    expect(screen.queryByRole('menuitem', { name: 'INSERT' })).toBeNull();
    expect(screen.queryByRole('menuitem', { name: 'UPDATE' })).toBeNull();
    expect(screen.queryByRole('menuitem', { name: 'DELETE' })).toBeNull();
  });
});

describe('"Script as" menu with Redis', () => {
  it('is hidden on key-value nodes', async () => {
    resetRedisMocks();
    const info = await openRedisSession();
    useSessionsStore.setState({ sessionsByConnection: { [info.connectionId]: info }, opening: {} });
    renderKv(<ObjectTree connectionId={info.connectionId} />);
    await expand('0');
    fireEvent.contextMenu(await item('cache:'));
    await screen.findByRole('menuitem', { name: 'Filter by pattern...' });
    expect(screen.queryByRole('menuitem', { name: 'Script as' })).toBeNull();
    expect(screen.queryByRole('menuitem', { name: 'Copy script as' })).toBeNull();
  });
});
