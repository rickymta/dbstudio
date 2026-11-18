import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import { polyfillLayoutApis } from '@/features/data/testUtils';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { mockLeaseSessionId } from '@/lib/mock/leases';
import { useDebugStore } from '@/features/debugger/debugStore';
import { useToastStore } from '@/components/ui/toastStore';
import { useSessionsStore } from '@/stores/sessions';
import { editorTabId, useTabsStore } from '@/stores/tabs';
import { EditorTab } from './EditorTab';
import { getTabData, patchTabData } from './editorStore';
import { runText } from './editorRunner';
import { openEditor } from './open';
import { MYSQL_STAGING, PG_DEV, PG_PROD_RO, openSession, resetEditorWorld } from './testUtils';

// Cold start (CodeMirror, react-query, Radix) can be slow on CI.
vi.setConfig({ testTimeout: 30_000 });

beforeAll(polyfillLayoutApis);
beforeEach(async () => {
  await i18n.changeLanguage('en');
  resetEditorWorld();
});
afterEach(() => vi.restoreAllMocks());

async function mount(text = 'SELECT 1') {
  const session = await openSession(PG_DEV);
  const id = openEditor(session, { text });
  const tab = useTabsStore.getState().tabs.find((t) => t.id === editorTabId(id))!;
  if (tab.kind !== 'editor') throw new Error('not an editor tab');
  render(
    <Providers>
      <EditorTab {...tab.payload} />
    </Providers>,
  );
  await screen.findByTestId('sql-editor', undefined, { timeout: 5000 });
  return { id, session };
}

const payloadOf = (id: string) => {
  const tab = useTabsStore.getState().tabs.find((t) => t.id === editorTabId(id))!;
  if (tab.kind !== 'editor') throw new Error('not an editor tab');
  return tab.payload;
};

/** Radix opens its menus on a real pointer event, which jsdom lacks: use the keyboard. */
const openByKeyboard = async (name: string) => {
  (await screen.findByRole('button', { name })).focus();
  await userEvent.keyboard('{Enter}');
  return screen.findByRole('menu');
};
const openMenu = () => openByKeyboard('Active connection');

describe('connection picker', () => {
  it('lists open connections first, then the other saved ones', async () => {
    await openSession(MYSQL_STAGING);
    await mount();
    const menu = await openMenu();
    const labels = within(menu)
      .getAllByRole('menuitemradio')
      .map((i) => i.textContent);
    expect(labels[0]).toContain('PostgreSQL dev');
    expect(labels[1]).toContain('MySQL staging');
    expect(labels[2]).toContain('PostgreSQL prod (read-only)');
    expect(within(menu).getByText('Open')).toBeInTheDocument();
    expect(within(menu).getByText('Other connections')).toBeInTheDocument();
    expect(within(menu).getByRole('menuitemradio', { name: /PostgreSQL dev/ })).toBeChecked();
    await userEvent.type(within(menu).getByRole('searchbox'), 'prod');
    expect(within(menu).getAllByRole('menuitemradio')).toHaveLength(1);
  });

  it('rebinds the tab to another open connection and runs on its session', async () => {
    const mysql = await openSession(MYSQL_STAGING);
    const { id, session } = await mount();
    await runText(id, session.sessionId, { text: 'SELECT 1', baseChar: 0 });
    const oldLease = getTabData(id)!.leaseId!;
    const close = vi.spyOn(ipc, 'leaseClose');
    patchTabData(id, { database: 'x', schema: 'y' });
    const menu = await openMenu();
    await userEvent.click(
      await within(menu).findByRole('menuitemradio', { name: /MySQL staging/ }),
    );
    await waitFor(() => expect(payloadOf(id).connectionId).toBe(MYSQL_STAGING));
    expect(payloadOf(id).sessionId).toBe(mysql.sessionId);
    expect(close).toHaveBeenCalledWith(oldLease);
    expect(getTabData(id)).toMatchObject({ leaseId: null, database: null, schema: null });
    expect(getTabData(id)!.text).toBe('SELECT 1');
    await runText(id, mysql.sessionId, { text: 'SELECT 2', baseChar: 0 });
    expect(mockLeaseSessionId(getTabData(id)!.leaseId!)).toBe(mysql.sessionId);
  });

  it('opens the session of a connection that is not connected yet', async () => {
    const { id } = await mount();
    const open = vi.spyOn(ipc, 'sessionOpen');
    const menu = await openMenu();
    await userEvent.click(
      await within(menu).findByRole('menuitemradio', { name: /PostgreSQL prod/ }),
    );
    await waitFor(() => expect(open).toHaveBeenCalled());
    expect(open.mock.calls[0]![0]).toBe(PG_PROD_RO);
    await waitFor(() =>
      expect(useSessionsStore.getState().sessionsByConnection[PG_PROD_RO]).toBeDefined(),
    );
    await waitFor(() =>
      expect(payloadOf(id).sessionId).toBe(
        useSessionsStore.getState().sessionsByConnection[PG_PROD_RO]!.sessionId,
      ),
    );
    expect(payloadOf(id).connectionId).toBe(PG_PROD_RO);
  });

  it('asks for the password of a connection that stores none, then rebinds', async () => {
    const { id } = await mount();
    const open = vi.spyOn(ipc, 'sessionOpen');
    const menu = await openMenu();
    await userEvent.click(
      await within(menu).findByRole('menuitemradio', { name: /MySQL staging/ }),
    );
    const dialog = await screen.findByRole('dialog');
    expect(open).not.toHaveBeenCalled();
    await userEvent.type(within(dialog).getByLabelText('Password'), 'secret');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Connect' }));
    await waitFor(() => expect(open).toHaveBeenCalledWith(MYSQL_STAGING, 'secret'));
    await waitFor(() =>
      expect(payloadOf(id).sessionId).toBe(
        useSessionsStore.getState().sessionsByConnection[MYSQL_STAGING]!.sessionId,
      ),
    );
    expect(payloadOf(id).connectionId).toBe(MYSQL_STAGING);
  });

  it('stays on the new connection, without an error toast, when the password prompt is cancelled', async () => {
    const { id } = await mount();
    const open = vi.spyOn(ipc, 'sessionOpen');
    const menu = await openMenu();
    await userEvent.click(
      await within(menu).findByRole('menuitemradio', { name: /MySQL staging/ }),
    );
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    expect(open).not.toHaveBeenCalled();
    expect(payloadOf(id)).toMatchObject({ connectionId: MYSQL_STAGING, sessionId: null });
    expect(useToastStore.getState().toasts).toHaveLength(0);
  });

  it('is disabled while a debug session is running or starting', async () => {
    const { id } = await mount();
    const trigger = await screen.findByRole('button', { name: 'Active connection' });
    const database = await screen.findByRole('button', { name: 'Active database' });
    expect(trigger).toBeEnabled();
    act(() => useDebugStore.getState().setStarting(id, true));
    await waitFor(() => expect(trigger).toBeDisabled());
    expect(database).toBeDisabled();
    expect(trigger).toHaveAttribute('title', expect.stringContaining('debug'));
    act(() => useDebugStore.getState().setStarting(id, false));
    await waitFor(() => expect(trigger).toBeEnabled());
  });

  it('is disabled while a transaction is open', async () => {
    const { id, session } = await mount('BEGIN');
    await runText(id, session.sessionId, { text: 'BEGIN', baseChar: 0 });
    expect(getTabData(id)!.leaseState!.transaction).toBe('active');
    const trigger = await screen.findByRole('button', { name: 'Active connection' });
    await waitFor(() => expect(trigger).toBeDisabled());
    expect(trigger).toHaveAttribute('title', expect.stringContaining('transaction'));
  });
});

describe('database picker', () => {
  /**
   * Name of a database other than the current one. The current one is only checked once the
   * connection profile (its default database) has loaded: before that no item is checked, and
   * picking the "first unchecked" item could pick the current database, which is a no-op.
   */
  const otherDatabase = async (menu: HTMLElement) => {
    await waitFor(
      () =>
        expect(
          within(menu)
            .getAllByRole('menuitemradio')
            .some((o) => o.getAttribute('aria-checked') === 'true'),
        ).toBe(true),
      { timeout: 5000 },
    );
    return within(menu)
      .getAllByRole('menuitemradio')
      .find((o) => o.getAttribute('aria-checked') === 'false')!.textContent!;
  };
  const pick = async (menu: HTMLElement, name: string) =>
    userEvent.click(within(menu).getByRole('menuitemradio', { name }));

  it('only patches the tab when it has no lease', async () => {
    const { id } = await mount();
    const set = vi.spyOn(ipc, 'leaseSetContext');
    const menu = await openByKeyboard('Active database');
    const name = await otherDatabase(menu);
    await pick(menu, name);
    await waitFor(() => expect(getTabData(id)!.database).toBe(name));
    expect(set).not.toHaveBeenCalled();
  });

  it('sets the context of the lease when it has one', async () => {
    const { id, session } = await mount();
    await runText(id, session.sessionId, { text: 'SELECT 1', baseChar: 0 });
    // Wait until the lease and the finished run are rendered (the picker is enabled again).
    await waitFor(() => expect(getTabData(id)!.leaseId).toBeTruthy());
    const leaseId = getTabData(id)!.leaseId!;
    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Active database' })).toBeEnabled(),
    );
    const set = vi.spyOn(ipc, 'leaseSetContext');
    const menu = await openByKeyboard('Active database');
    const name = await otherDatabase(menu);
    await pick(menu, name);
    await waitFor(() =>
      expect(set).toHaveBeenCalledWith(leaseId, { database: name, schema: null }),
    );
    await waitFor(() => expect(getTabData(id)!.leaseState!.database).toBe(name));
    expect(getTabData(id)!.database).toBe(name);
  });
});
