import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import { Toaster } from '@/components/ui/Toast';
import { useToastStore } from '@/components/ui/toastStore';
import { pickFile, pickSavePath } from '@/lib/dialog';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import type { ObjectKind } from '@/lib/types';
import { resetMockConnections } from '@/lib/mock';
import { useSessionsStore } from '@/stores/sessions';
import { useTabsStore, WELCOME_TAB } from '@/stores/tabs';
import { ConnectionsSidebar } from './ConnectionsSidebar';

vi.mock('@/lib/dialog', () => ({ pickFile: vi.fn(), pickSavePath: vi.fn() }));

const PG_DEV = '00000000-0000-4000-8000-0000000000b1';
const MYSQL_STAGING = '00000000-0000-4000-8000-0000000000b2';
const WAIT = { timeout: 4000 };

beforeEach(async () => {
  resetMockConnections();
  useToastStore.setState({ toasts: [] });
  localStorage.clear();
  useSessionsStore.setState({ sessionsByConnection: {}, opening: {} });
  useTabsStore.setState({ tabs: [WELCOME_TAB], activeId: WELCOME_TAB.id });
  vi.restoreAllMocks();
  vi.mocked(pickFile).mockReset();
  vi.mocked(pickSavePath).mockReset();
  await i18n.changeLanguage('en');
});

function setup() {
  const user = userEvent.setup();
  render(
    <Providers>
      <ConnectionsSidebar />
      <Toaster />
    </Providers>,
  );
  return user;
}

const profileRow = (name: string) =>
  screen.findByRole('treeitem', { name }, WAIT) as Promise<HTMLElement>;

async function openMenu(row: HTMLElement) {
  fireEvent.contextMenu(row);
  await screen.findByRole('menu');
}

async function menuAction(
  row: HTMLElement,
  name: string | RegExp,
  user: ReturnType<typeof userEvent.setup>,
) {
  await openMenu(row);
  await user.click(await screen.findByRole('menuitem', { name }));
}

async function newConnection(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getByRole('button', { name: 'New connection' }));
  return screen.findByRole('dialog', { name: 'New connection' });
}

describe('ConnectionsSidebar', () => {
  it('lists groups and profiles from the mock backend', async () => {
    setup();
    expect(await profileRow('PostgreSQL dev')).toBeInTheDocument();
    expect(screen.getByRole('treeitem', { name: 'Work' })).toBeInTheDocument();
    expect(
      screen.getByRole('treeitem', { name: 'PostgreSQL prod (read-only)' }),
    ).toBeInTheDocument();
  });

  it('creates a connection through the dialog and shows it in the sidebar', async () => {
    const user = setup();
    await profileRow('PostgreSQL dev');
    const dialog = await newConnection(user);
    await user.type(within(dialog).getByLabelText('Name'), 'My local');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    expect(await profileRow('My local')).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  });

  it('saves with Enter and closes with Escape', async () => {
    const user = setup();
    await profileRow('PostgreSQL dev');
    const dialog = await newConnection(user);
    await user.type(within(dialog).getByLabelText('Name'), 'Enter saved{Enter}');
    expect(await profileRow('Enter saved')).toBeInTheDocument();
    await newConnection(user);
    await user.keyboard('{Escape}');
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  });

  it('validates name, host and port in place', async () => {
    const user = setup();
    await profileRow('PostgreSQL dev');
    const dialog = await newConnection(user);
    await user.clear(within(dialog).getByLabelText('Host'));
    await user.clear(within(dialog).getByLabelText('Port'));
    await user.type(within(dialog).getByLabelText('Port'), '99999');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    expect(await within(dialog).findByText('Name is required')).toBeInTheDocument();
    expect(within(dialog).getByText('Host is required')).toBeInTheDocument();
    expect(within(dialog).getByText(/Port must be a number/)).toBeInTheDocument();
    expect(within(dialog).getByLabelText('Name')).toHaveAttribute('aria-invalid', 'true');
  });

  it('fills the default port when the type changes, unless typed by hand', async () => {
    const user = setup();
    await profileRow('PostgreSQL dev');
    const dialog = await newConnection(user);
    const port = within(dialog).getByLabelText('Port') as HTMLInputElement;
    expect(port.value).toBe('5432');
    await user.click(within(dialog).getByRole('combobox', { name: 'Database type' }));
    // Planned drivers are listed but cannot be chosen.
    expect(await screen.findByRole('option', { name: 'Oracle (Phase 3)' })).toHaveAttribute(
      'aria-disabled',
      'true',
    );
    await user.click(screen.getByRole('option', { name: 'MySQL' }));
    await waitFor(() => expect(port.value).toBe('3306'));
    await user.clear(port);
    await user.type(port, '3999');
    await user.click(within(dialog).getByRole('combobox', { name: 'Database type' }));
    await user.click(await screen.findByRole('option', { name: 'PostgreSQL' }));
    expect(port.value).toBe('3999');
  });

  it('tests a connection: success shows server product and version', async () => {
    const user = setup();
    await profileRow('PostgreSQL dev');
    const dialog = await newConnection(user);
    await user.click(within(dialog).getByRole('button', { name: 'Test connection' }));
    expect(
      await within(dialog).findByText('Connected to PostgreSQL 17.2', {}, WAIT),
    ).toBeInTheDocument();
  });

  it('tests a connection: failure shows the classified error', async () => {
    const user = setup();
    await profileRow('PostgreSQL dev');
    const dialog = await newConnection(user);
    const host = within(dialog).getByLabelText('Host');
    await user.clear(host);
    await user.type(host, 'fail.example.com');
    await user.click(within(dialog).getByRole('button', { name: 'Test connection' }));
    const alert = await within(dialog).findByRole('alert', {}, WAIT);
    expect(alert).toHaveTextContent(
      'Connection failed: could not connect to server at fail.example.com',
    );
  });

  it('shows TLS file fields only for verifying modes', async () => {
    const user = setup();
    await profileRow('PostgreSQL dev');
    const dialog = await newConnection(user);
    expect(within(dialog).queryByLabelText('CA certificate')).toBeNull();
    await user.click(within(dialog).getByRole('combobox', { name: 'TLS mode' }));
    await user.click(await screen.findByRole('option', { name: 'Verify CA' }));
    expect(within(dialog).getByLabelText('CA certificate')).toBeInTheDocument();
    expect(within(dialog).getByLabelText('Client key')).toBeInTheDocument();
  });

  it('editing keeps the stored password unless a new one is typed or it is cleared', async () => {
    const save = vi.spyOn(ipc, 'connectionSave');
    const user = setup();
    const row = await profileRow('PostgreSQL dev');
    await menuAction(row, 'Edit...', user);
    let dialog = await screen.findByRole('dialog', { name: 'Edit connection' });
    expect(within(dialog).getByLabelText('Name')).toHaveValue('PostgreSQL dev');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    expect(save.mock.calls[0]![1]).toEqual({ action: 'keep' });
    expect(save.mock.calls[0]![0].id).toBe(PG_DEV);
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());

    await menuAction(await profileRow('PostgreSQL dev'), 'Edit...', user);
    dialog = await screen.findByRole('dialog', { name: 'Edit connection' });
    await user.type(within(dialog).getByLabelText('Password'), 'secret');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(save).toHaveBeenCalledTimes(2));
    expect(save.mock.calls[1]![1]).toEqual({ action: 'set', value: 'secret' });
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());

    await menuAction(await profileRow('PostgreSQL dev'), 'Edit...', user);
    dialog = await screen.findByRole('dialog', { name: 'Edit connection' });
    await user.click(within(dialog).getByRole('button', { name: 'Remove saved password' }));
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(save).toHaveBeenCalledTimes(3));
    expect(save.mock.calls[2]![1]).toEqual({ action: 'clear' });
  });

  it('shows a save error inside the dialog and keeps it open', async () => {
    vi.spyOn(ipc, 'connectionSave').mockRejectedValue({ kind: 'storage', message: 'disk full' });
    const user = setup();
    await profileRow('PostgreSQL dev');
    const dialog = await newConnection(user);
    await user.type(within(dialog).getByLabelText('Name'), 'Doomed');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    expect(await within(dialog).findByText('Storage error: disk full')).toBeInTheDocument();
    expect(screen.getByRole('dialog')).toBeInTheDocument();
    expect(within(dialog).getByRole('button', { name: 'Save' })).toBeEnabled();
  });

  it('closes the dialog and warns when the profile was saved despite a storage error', async () => {
    const original = ipc.connectionSave.bind(ipc);
    vi.spyOn(ipc, 'connectionSave').mockImplementation(async (p, secret) => {
      await original(p, secret);
      throw {
        kind: 'storage',
        message: 'connection saved, but its password could not be stored: keychain',
      };
    });
    const user = setup();
    await profileRow('PostgreSQL dev');
    const dialog = await newConnection(user);
    await user.type(within(dialog).getByLabelText('Name'), 'Half saved');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    expect(await profileRow('Half saved')).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(await screen.findByRole('alert')).toHaveTextContent('password could not be stored');
  });

  it('duplicates a connection', async () => {
    const user = setup();
    await menuAction(await profileRow('PostgreSQL dev'), 'Duplicate', user);
    expect(await profileRow('PostgreSQL dev (copy)')).toBeInTheDocument();
  });

  it('deletes a connection only after confirmation, disconnecting an open one', async () => {
    const user = setup();
    const row = await profileRow('PostgreSQL prod (read-only)');
    await menuAction(row, 'Delete', user);
    const dialog = await screen.findByRole('dialog');
    expect(dialog).toHaveTextContent('Delete connection "PostgreSQL prod (read-only)"?');
    await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(
      screen.getByRole('treeitem', { name: 'PostgreSQL prod (read-only)' }),
    ).toBeInTheDocument();

    // Connect PostgreSQL dev, then delete it.
    const dev = await profileRow('PostgreSQL dev');
    await user.dblClick(dev);
    await waitFor(
      () => expect(useSessionsStore.getState().sessionsByConnection[PG_DEV]).toBeDefined(),
      WAIT,
    );
    await menuAction(dev, 'Delete', user);
    const confirm = await screen.findByRole('dialog');
    expect(confirm).toHaveTextContent(/disconnected/);
    await user.click(within(confirm).getByRole('button', { name: 'Delete' }));
    await waitFor(
      () => expect(screen.queryByRole('treeitem', { name: 'PostgreSQL dev' })).toBeNull(),
      WAIT,
    );
    expect(useSessionsStore.getState().sessionsByConnection[PG_DEV]).toBeUndefined();
  });

  it('creates, renames and deletes groups; profiles move up on delete', async () => {
    const user = setup();
    await profileRow('PostgreSQL dev');
    await user.click(screen.getByRole('button', { name: 'New group' }));
    let dialog = await screen.findByRole('dialog', { name: 'New group' });
    await user.type(within(dialog).getByLabelText('Group name'), 'Ops{Enter}');
    expect(await screen.findByRole('treeitem', { name: 'Ops' }, WAIT)).toBeInTheDocument();

    await menuAction(screen.getByRole('treeitem', { name: 'Ops' }), 'Rename...', user);
    dialog = await screen.findByRole('dialog', { name: 'Rename group' });
    const input = within(dialog).getByLabelText('Group name');
    await user.clear(input);
    await user.type(input, 'Operations{Enter}');
    expect(await screen.findByRole('treeitem', { name: 'Operations' }, WAIT)).toBeInTheDocument();
    expect(screen.queryByRole('treeitem', { name: 'Ops' })).toBeNull();

    await menuAction(screen.getByRole('treeitem', { name: 'Work' }), 'Delete', user);
    dialog = await screen.findByRole('dialog');
    expect(dialog).toHaveTextContent(/move up to the parent group/);
    await user.click(within(dialog).getByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(screen.queryByRole('treeitem', { name: 'Work' })).toBeNull(), WAIT);
    expect(await profileRow('PostgreSQL dev')).toBeInTheDocument();
  });

  it('moves a profile into a group and out again', async () => {
    const save = vi.spyOn(ipc, 'connectionSave');
    const user = setup();
    const row = await profileRow('PostgreSQL prod (read-only)');
    await openMenu(row);
    await user.hover(await screen.findByRole('menuitem', { name: 'Move to group' }));
    // The submenu sits outside the pointer-events scope jsdom computes, so click it directly.
    fireEvent.click(await screen.findByRole('menuitem', { name: 'Work' }));
    await waitFor(() => expect(save).toHaveBeenCalledTimes(1), WAIT);
    expect(save.mock.calls[0]![0]).toMatchObject({
      name: 'PostgreSQL prod (read-only)',
      groupId: expect.any(String),
    });
    expect(save.mock.calls[0]![1]).toEqual({ action: 'keep' });
    // It now sits under the Work group.
    await waitFor(async () => {
      const profiles = (await ipc.connectionsList()).profiles;
      expect(profiles.find((p) => p.name.startsWith('PostgreSQL prod'))?.groupId).toBeTruthy();
    }, WAIT);
  });

  it('remembers collapsed groups in localStorage', async () => {
    const user = setup();
    await profileRow('PostgreSQL dev');
    await user.click(screen.getByRole('treeitem', { name: 'Work' }));
    expect(screen.queryByRole('treeitem', { name: 'PostgreSQL dev' })).toBeNull();
    expect(localStorage.getItem('dbstudio.connections.collapsedGroups')).toContain('00000000');
    await user.click(screen.getByRole('treeitem', { name: 'Work' }));
    expect(await profileRow('PostgreSQL dev')).toBeInTheDocument();
  });

  it('connects, expands the tree down to a table and opens its data tab', async () => {
    const user = setup();
    await user.dblClick(await profileRow('PostgreSQL dev'));
    const info = await waitFor(() => {
      const s = useSessionsStore.getState().sessionsByConnection[PG_DEV];
      expect(s).toBeDefined();
      return s!;
    }, WAIT);

    for (const name of ['app_dev', 'Schemas', 'public', 'Tables']) {
      await user.dblClick(await screen.findByText(name, {}, WAIT));
    }
    const treeitem = await screen.findByRole('treeitem', { name: /^users/ }, WAIT);
    expect(screen.getByRole('treeitem', { name: /^Tables/ })).toHaveAttribute(
      'aria-expanded',
      'true',
    );
    await user.dblClick(treeitem);

    const { tabs, activeId } = useTabsStore.getState();
    const tab = tabs.find((t) => t.kind === 'table');
    expect(tab).toBeDefined();
    expect(activeId).toBe(tab!.id);
    expect(tab).toMatchObject({
      title: 'users',
      closable: true,
      payload: {
        sessionId: info.sessionId,
        connectionId: PG_DEV,
        path: {
          segments: [
            { kind: 'database', name: 'app_dev' },
            { kind: 'folder', name: 'schemas' },
            { kind: 'schema', name: 'public' },
            { kind: 'folder', name: 'tables' },
            { kind: 'table', name: 'users' },
          ],
        },
      },
    });
  });

  it('supports the keyboard in the tree and opens DDL tabs for routines', async () => {
    const user = setup();
    await user.dblClick(await profileRow('PostgreSQL dev'));
    const tree = await screen.findByTestId('object-tree', {}, WAIT);
    await screen.findByText('app_dev', {}, WAIT);
    tree.focus();
    await user.keyboard('{ArrowRight}'); // first node (app_dev) is focused: expand it
    await screen.findByText('Schemas', {}, WAIT);
    await user.keyboard('{ArrowRight}{ArrowRight}'); // into Schemas, expand it
    await screen.findByText('public', {}, WAIT);
    await user.keyboard('{ArrowRight}{ArrowRight}'); // into public, expand it
    await screen.findByText('Functions', {}, WAIT);
    // Tables, Views, Materialized views, then Functions.
    await user.keyboard('{ArrowRight}{ArrowDown}{ArrowDown}{ArrowDown}{ArrowRight}');
    await screen.findByText('slugify', {}, WAIT);
    await user.keyboard('{ArrowRight}{ArrowDown}{Enter}');
    const tab = useTabsStore.getState().tabs.find((t) => t.kind === 'ddl');
    expect(tab).toMatchObject({ title: 'slugify', kind: 'ddl' });
    // Left goes back to the parent folder.
    await user.keyboard('{ArrowLeft}');
    expect(tree.getAttribute('aria-activedescendant')).toContain('functions');
  });

  it('refresh reloads a branch from the backend', async () => {
    const spy = vi.spyOn(ipc, 'objectChildren');
    const user = setup();
    await user.dblClick(await profileRow('PostgreSQL dev'));
    await user.dblClick(await screen.findByText('app_dev', {}, WAIT));
    await screen.findByText('Schemas', {}, WAIT);
    const callsBefore = spy.mock.calls.length;
    await menuAction(screen.getByRole('treeitem', { name: /^app_dev/ }), 'Refresh', user);
    await waitFor(() => expect(spy.mock.calls.length).toBeGreaterThan(callsBefore), WAIT);
  });

  it('removes the tree when the session closes', async () => {
    const user = setup();
    await user.dblClick(await profileRow('PostgreSQL dev'));
    await screen.findByTestId('object-tree', {}, WAIT);
    await menuAction(screen.getByRole('treeitem', { name: 'PostgreSQL dev' }), 'Disconnect', user);
    await waitFor(() => expect(screen.queryByTestId('object-tree')).toBeNull(), WAIT);
  });

  it('asks for the password of a profile that does not save one', async () => {
    const user = setup();
    await user.dblClick(await profileRow('MySQL staging'));
    const dialog = await screen.findByRole('dialog', { name: 'Password required' });
    expect(useSessionsStore.getState().sessionsByConnection[MYSQL_STAGING]).toBeUndefined();
    await user.type(within(dialog).getByLabelText('Password'), 'hunter2{Enter}');
    await waitFor(
      () => expect(useSessionsStore.getState().sessionsByConnection[MYSQL_STAGING]).toBeDefined(),
      WAIT,
    );
  });

  it('does not connect when the password prompt is cancelled', async () => {
    const user = setup();
    await user.dblClick(await profileRow('MySQL staging'));
    const dialog = await screen.findByRole('dialog', { name: 'Password required' });
    await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(useSessionsStore.getState().sessionsByConnection[MYSQL_STAGING]).toBeUndefined();
  });

  it('reports a connection failure with a toast and a retry button on the row', async () => {
    const base = (await ipc.connectionsList()).profiles[0]!;
    await ipc.connectionSave(
      { ...base, id: crypto.randomUUID(), name: 'Broken', host: 'fail-host', hasPassword: true },
      { action: 'set', value: 'x' },
    );
    const user = setup();
    await user.dblClick(await profileRow('Broken'));
    const alert = await screen.findByRole('alert', {}, WAIT);
    expect(alert).toHaveTextContent(
      'Broken: Connection failed: could not connect to server at fail-host',
    );
    expect(await screen.findByRole('button', { name: 'Retry connecting' })).toBeInTheDocument();
  });

  it('filters profiles and groups by name', async () => {
    const user = setup();
    await profileRow('PostgreSQL dev');
    await user.type(
      screen.getByRole('searchbox', { name: 'Filter connections and objects' }),
      'staging',
    );
    expect(screen.getByRole('treeitem', { name: 'MySQL staging' })).toBeInTheDocument();
    expect(screen.queryByRole('treeitem', { name: 'PostgreSQL dev' })).toBeNull();
    expect(screen.queryByRole('treeitem', { name: 'PostgreSQL prod (read-only)' })).toBeNull();
    await user.clear(screen.getByRole('searchbox'));
    expect(await profileRow('PostgreSQL dev')).toBeInTheDocument();
  });

  it('filters already loaded tree nodes, keeping their ancestors', async () => {
    const user = setup();
    await user.dblClick(await profileRow('PostgreSQL dev'));
    for (const name of ['app_dev', 'Schemas', 'public', 'Tables']) {
      await user.dblClick(await screen.findByText(name, {}, WAIT));
    }
    await screen.findByText('order_items', {}, WAIT);
    await user.type(screen.getByRole('searchbox'), 'order_items');
    await waitFor(() => expect(screen.queryByText('products')).toBeNull());
    expect(screen.getByText('order_items')).toBeInTheDocument();
    expect(screen.getByText('Tables')).toBeInTheDocument();
    expect(screen.getByRole('treeitem', { name: 'PostgreSQL dev' })).toBeInTheDocument();
    expect(screen.queryByRole('treeitem', { name: 'MySQL staging' })).toBeNull();
  });

  it('imports and exports through the native file dialogs', async () => {
    const importSpy = vi.spyOn(ipc, 'connectionsImport');
    const exportSpy = vi.spyOn(ipc, 'connectionsExport');
    vi.mocked(pickFile).mockResolvedValue('/tmp/in.json');
    vi.mocked(pickSavePath).mockResolvedValue('/tmp/out.json');
    const user = setup();
    await profileRow('PostgreSQL dev');
    await user.click(screen.getByRole('button', { name: 'More actions' }));
    await user.click(await screen.findByRole('menuitem', { name: 'Import connections...' }));
    await waitFor(() => expect(importSpy).toHaveBeenCalledWith('/tmp/in.json'));
    await user.click(screen.getByRole('button', { name: 'More actions' }));
    await user.click(await screen.findByRole('menuitem', { name: 'Export connections...' }));
    await waitFor(() => expect(exportSpy).toHaveBeenCalledWith('/tmp/out.json'));
    expect(await screen.findByText('Exported 3 connection(s)', {}, WAIT)).toBeInTheDocument();
  });

  it('collapses everything from the more-actions menu', async () => {
    const user = setup();
    await profileRow('PostgreSQL dev');
    await user.click(screen.getByRole('button', { name: 'More actions' }));
    await user.click(await screen.findByRole('menuitem', { name: 'Collapse all' }));
    await waitFor(() =>
      expect(screen.queryByRole('treeitem', { name: 'PostgreSQL dev' })).toBeNull(),
    );
  });

  it('shows the empty state with a create button when there are no connections', async () => {
    vi.spyOn(ipc, 'connectionsList').mockResolvedValue({ groups: [], profiles: [] });
    const user = setup();
    expect(await screen.findByText('No connections yet', {}, WAIT)).toBeInTheDocument();
    await user.click(screen.getAllByRole('button', { name: 'New connection' }).at(-1)!);
    expect(await screen.findByRole('dialog', { name: 'New connection' })).toBeInTheDocument();
  });

  it('shows a retryable error when the list cannot be loaded', async () => {
    vi.spyOn(ipc, 'connectionsList').mockRejectedValueOnce({ kind: 'storage', message: 'locked' });
    const user = setup();
    expect(await screen.findByText('Could not load connections', {}, WAIT)).toBeInTheDocument();
    expect(screen.getByText('locked')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Retry connecting' }));
    expect(await profileRow('PostgreSQL dev')).toBeInTheDocument();
  });

  it('ignores an unsafe profile color instead of putting it into CSS', async () => {
    const base = (await ipc.connectionsList()).profiles[2]!; // production profile
    await ipc.connectionSave(
      {
        ...base,
        id: crypto.randomUUID(),
        name: 'Painted',
        color: '#f00, 0 0 0 100vmax #000',
        groupId: null,
      },
      { action: 'keep' },
    );
    setup();
    const row = await profileRow('Painted');
    expect(row.style.boxShadow).not.toContain('100vmax');
    expect(row.style.boxShadow).toContain('var(--danger)');
  });

  it('survives an object kind it does not know', async () => {
    const original = ipc.objectChildren.bind(ipc);
    vi.spyOn(ipc, 'objectChildren').mockImplementation(async (sessionId, path) => {
      if (path.segments.length > 0) return original(sessionId, path);
      return [
        {
          path: { segments: [{ kind: 'hologram' as ObjectKind, name: 'holo' }] },
          kind: 'hologram' as ObjectKind,
          name: 'holo',
          hasChildren: false,
        },
      ];
    });
    const user = setup();
    await user.dblClick(await profileRow('PostgreSQL dev'));
    const holo = await screen.findByText('holo', {}, WAIT);
    await user.dblClick(holo);
    expect(useTabsStore.getState().tabs).toHaveLength(1);
    fireEvent.contextMenu(holo);
    const menu = await screen.findByRole('menu');
    expect(
      within(menu)
        .getAllByRole('menuitem')
        .map((i) => i.textContent),
    ).toEqual(['Copy name']);
  });

  it('keeps one valid tree and only points aria-activedescendant at rendered rows', async () => {
    const many = Array.from({ length: 300 }, (_, i) => ({
      path: { segments: [{ kind: 'database' as const, name: `db${i}` }] },
      kind: 'database' as const,
      name: `db${i}`,
      hasChildren: false,
    }));
    vi.spyOn(ipc, 'objectChildren').mockResolvedValue(many);
    const user = setup();
    await user.dblClick(await profileRow('PostgreSQL dev'));
    const tree = await screen.findByTestId('object-tree', {}, WAIT);
    await screen.findByText('db0', {}, WAIT);
    expect(screen.getAllByRole('tree')).toHaveLength(1);
    expect(tree).toHaveAttribute('role', 'group');
    // Virtualized: far fewer rows than nodes are mounted.
    expect(within(tree).getAllByRole('treeitem').length).toBeLessThan(100);
    await user.click(tree);
    await user.keyboard('{Home}');
    const active = () => tree.getAttribute('aria-activedescendant');
    await waitFor(() => expect(document.getElementById(active() ?? '')).not.toBeNull());
    await user.keyboard('{End}');
    // The focused last node is not mounted, so the attribute must be absent.
    expect(active()).toBeNull();
    await user.keyboard('{Home}');
    expect(document.getElementById(active() ?? '')).not.toBeNull();
  });

  it('warns about unverified TLS on remote hosts, with stronger emphasis for production', async () => {
    const user = setup();
    await profileRow('PostgreSQL dev');
    const dialog = await newConnection(user);
    const host = within(dialog).getByLabelText('Host');
    // Default host is localhost and the default mode is prefer: no warning.
    expect(within(dialog).queryByTestId('tls-warning')).toBeNull();
    await user.clear(host);
    await user.type(host, 'db.example.com');
    const warning = await within(dialog).findByTestId('tls-warning');
    expect(warning).toHaveTextContent(/fall back to unencrypted/);
    expect(warning).toHaveClass('text-warning');

    await user.click(within(dialog).getByRole('combobox', { name: 'Environment' }));
    await user.click(await screen.findByRole('option', { name: 'Production' }));
    expect(within(dialog).getByTestId('tls-warning')).toHaveClass('text-danger');

    await user.click(within(dialog).getByRole('combobox', { name: 'TLS mode' }));
    await user.click(await screen.findByRole('option', { name: 'Verify full (CA and host name)' }));
    expect(within(dialog).queryByTestId('tls-warning')).toBeNull();
    expect(within(dialog).getByText(/Recommended/)).toBeInTheDocument();

    await user.click(within(dialog).getByRole('combobox', { name: 'TLS mode' }));
    await user.click(await screen.findByRole('option', { name: 'Disable' }));
    expect(within(dialog).getByTestId('tls-warning')).toHaveTextContent(/not encrypted/);
    await user.clear(host);
    await user.type(host, 'localhost');
    expect(within(dialog).queryByTestId('tls-warning')).toBeNull();
  });
});
