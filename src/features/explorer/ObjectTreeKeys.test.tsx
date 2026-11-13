import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import {
  openRedisSession,
  renderWithProviders,
  resetRedisMocks,
} from '@/features/keyvalue/testUtils';
import { useSessionsStore } from '@/stores/sessions';
import { useTabsStore, WELCOME_TAB } from '@/stores/tabs';
import { ObjectTree } from './ObjectTree';

const WAIT = { timeout: 4000 };
let connectionId: string;

beforeEach(async () => {
  resetRedisMocks();
  useTabsStore.setState({ tabs: [WELCOME_TAB], activeId: WELCOME_TAB.id });
  const info = await openRedisSession();
  connectionId = info.connectionId;
  useSessionsStore.setState({ sessionsByConnection: { [connectionId]: info }, opening: {} });
});

const item = (name: string | RegExp) => screen.findByRole('treeitem', { name }, WAIT);

async function expand(name: string | RegExp) {
  const row = await item(name);
  fireEvent.click(within(row).getByRole('button', { name: 'Expand' }));
}

describe('ObjectTree with a paged key-value tree', () => {
  it('shows "Load more" and appends pages without repeating prefixes', async () => {
    renderWithProviders(<ObjectTree connectionId={connectionId} />);
    await expand('0');
    const more = await screen.findByRole('button', { name: /Load more/ }, WAIT);
    expect(more).toBeInTheDocument();
    // The first batch of keys only holds cache:* keys.
    expect(await item('cache:')).toBeInTheDocument();
    expect(screen.queryByRole('treeitem', { name: /^user:/ })).toBeNull();

    for (let i = 0; i < 400; i++) {
      const btn = screen.queryByRole('button', { name: /Load more/ });
      if (!btn) break;
      fireEvent.click(btn);
      await new Promise((r) => setTimeout(r, 20));
    }
    await waitFor(
      () => expect(screen.queryByRole('button', { name: /Load more/ })).toBeNull(),
      WAIT,
    );
    expect(screen.getAllByRole('treeitem', { name: /^cache:/ })).toHaveLength(1);
    expect(await item(/^user:/)).toBeInTheDocument();
    expect(await item(/^configstring/)).toBeInTheDocument();
  });

  it('shows the key type as a badge and opens a key tab on double click', async () => {
    renderWithProviders(<ObjectTree connectionId={connectionId} />);
    await expand('1');
    await expand(/^jobs:/);
    const row = await item(/^lock/);
    expect(within(row).getByTestId('key-type-badge')).toHaveTextContent('string');
    fireEvent.doubleClick(row);
    const tab = useTabsStore.getState().tabs.find((t) => t.kind === 'key');
    expect(tab).toMatchObject({ kind: 'key', payload: { connectionId } });
    expect(tab?.kind === 'key' && tab.payload.path.segments.at(-1)).toEqual({
      kind: 'key',
      name: 'jobs:lock',
    });
  });

  it('filters a level by glob pattern from the context menu', async () => {
    const user = userEvent.setup();
    renderWithProviders(<ObjectTree connectionId={connectionId} />);
    await expand('0');
    fireEvent.contextMenu(await item('cache:'));
    await user.click(await screen.findByRole('menuitem', { name: 'Filter by pattern...' }));
    const input = await screen.findByLabelText('Pattern');
    await user.clear(input);
    await user.type(input, 'leaderboard:*');
    await user.click(screen.getByRole('button', { name: 'Apply' }));
    expect(await screen.findByText('Filter: leaderboard:*', undefined, WAIT)).toBeInTheDocument();
  });

  it('offers the delete-by-pattern entry on key prefixes', async () => {
    renderWithProviders(<ObjectTree connectionId={connectionId} />);
    await expand('0');
    fireEvent.contextMenu(await item('cache:'));
    expect(
      await screen.findByRole('menuitem', { name: 'Delete keys by pattern...' }),
    ).toBeInTheDocument();
  });
});
