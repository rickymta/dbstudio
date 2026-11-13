// Disconnecting asks the tabs of the session first: an open transaction or pending edits veto it.
import { act, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import { polyfillLayoutApis } from '@/features/data/testUtils';
import '@/features/data/edit/pendingGuard';
import { usePendingChanges } from '@/features/data/edit/pendingChanges';
import i18n from '@/lib/i18n';
import { ROOT_PATH, childPath } from '@/lib/objectPath';
import { useSessionsStore } from '@/stores/sessions';
import { tableTabId, useTabsStore } from '@/stores/tabs';
import { EditorHost } from './EditorHost';
import { getTabData } from './editorStore';
import { runText } from './editorRunner';
import { openEditor } from './open';
import { PG_DEV, openSession, resetEditorWorld } from './testUtils';

beforeAll(polyfillLayoutApis);
beforeEach(async () => {
  await i18n.changeLanguage('en');
  resetEditorWorld();
  usePendingChanges.setState({ byTab: {} });
});
afterEach(() => vi.restoreAllMocks());

const connected = () => useSessionsStore.getState().sessionsByConnection[PG_DEV];

describe('disconnect with unfinished work', () => {
  it('asks before closing a session whose editor has a transaction; keeping it does not disconnect', async () => {
    const user = userEvent.setup();
    const session = await openSession();
    const id = openEditor(session, { text: 'BEGIN' });
    await runText(id, session.sessionId, { text: 'BEGIN', baseChar: 0 });
    expect(getTabData(id)!.leaseState!.transaction).toBe('active');
    render(
      <Providers>
        <EditorHost />
      </Providers>,
    );
    let first: Promise<boolean> | undefined;
    act(() => {
      first = useSessionsStore.getState().requestClose(PG_DEV);
    });
    let dialog = await screen.findByRole('dialog', { name: 'A transaction is still open' });
    await user.click(within(dialog).getByRole('button', { name: 'Keep tab open' }));
    expect(await first).toBe(false);
    expect(connected()).toBeDefined();

    let second: Promise<boolean> | undefined;
    act(() => {
      second = useSessionsStore.getState().requestClose(PG_DEV);
    });
    dialog = await screen.findByRole('dialog', { name: 'A transaction is still open' });
    await user.click(within(dialog).getByRole('button', { name: 'Roll back and close' }));
    expect(await second).toBe(true);
    expect(connected()).toBeUndefined();
  });

  it('asks before closing a session with a data tab that has pending changes', async () => {
    const session = await openSession();
    const path = childPath(childPath(ROOT_PATH, 'database', 'app'), 'table', 'users');
    const tabId = tableTabId(session.sessionId, path);
    useTabsStore.getState().openTab({
      id: tabId,
      kind: 'table',
      title: 'users',
      closable: true,
      payload: { sessionId: session.sessionId, connectionId: PG_DEV, path },
    });
    usePendingChanges.getState().toggleDelete(tabId, [{ t: 'int', v: 1 }]);
    const confirm = vi.spyOn(window, 'confirm').mockReturnValueOnce(false);
    expect(await useSessionsStore.getState().requestClose(PG_DEV)).toBe(false);
    expect(confirm).toHaveBeenCalledTimes(1);
    expect(connected()).toBeDefined();
    expect(useTabsStore.getState().tabs.some((t) => t.id === tabId)).toBe(true);

    confirm.mockReturnValueOnce(true);
    expect(await useSessionsStore.getState().requestClose(PG_DEV)).toBe(true);
    expect(connected()).toBeUndefined();
    expect(useTabsStore.getState().tabs.some((t) => t.id === tabId)).toBe(false);
  });

  it('closes without asking when no tab has unfinished work', async () => {
    await openSession();
    const confirm = vi.spyOn(window, 'confirm');
    expect(await useSessionsStore.getState().requestClose(PG_DEV)).toBe(true);
    expect(confirm).not.toHaveBeenCalled();
    expect(connected()).toBeUndefined();
  });
});
