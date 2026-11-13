import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ipc } from '@/lib/ipc';
import { ROOT_PATH, childPath } from '@/lib/objectPath';
import { useSessionsStore } from './sessions';
import { WELCOME_TAB, tableTabId, useTabsStore, type TabItem } from './tabs';

const path = childPath(ROOT_PATH, 'database', 'app');
const tab = (sessionId: string, name: string): TabItem => ({
  id: tableTabId(sessionId, childPath(path, 'table', name)),
  kind: 'table',
  title: name,
  closable: true,
  payload: { sessionId, connectionId: 'c1', path: childPath(path, 'table', name) },
});

describe('tabs store', () => {
  beforeEach(() => useTabsStore.setState({ tabs: [WELCOME_TAB], activeId: 'welcome' }));

  it('opens, activates duplicates and closes with neighbor activation', () => {
    const { openTab, closeTab, activate } = useTabsStore.getState();
    openTab(tab('s1', 'a'));
    openTab(tab('s1', 'b'));
    openTab(tab('s1', 'a')); // duplicate id: only activates
    expect(useTabsStore.getState().tabs).toHaveLength(3);
    expect(useTabsStore.getState().activeId).toBe(tab('s1', 'a').id);
    closeTab('welcome'); // not closable
    expect(useTabsStore.getState().tabs).toHaveLength(3);
    activate(tab('s1', 'b').id);
    closeTab(tab('s1', 'b').id);
    expect(useTabsStore.getState().activeId).toBe(tab('s1', 'a').id);
  });

  it('closes all tabs of a session', () => {
    const { openTab, closeSessionTabs } = useTabsStore.getState();
    openTab(tab('s1', 'a'));
    openTab(tab('s2', 'b'));
    closeSessionTabs('s1');
    expect(useTabsStore.getState().tabs.map((t) => t.id)).toEqual(['welcome', tab('s2', 'b').id]);
  });
});

describe('sessions store (mock IPC)', () => {
  it('opens once per connection and closes its tabs', async () => {
    const id = '00000000-0000-4000-8000-0000000000b1';
    const [a, b] = await Promise.all([
      useSessionsStore.getState().open(id),
      useSessionsStore.getState().open(id),
    ]);
    expect(a.sessionId).toBe(b.sessionId);
    expect(useSessionsStore.getState().sessionsByConnection[id]).toBeDefined();
    useTabsStore.getState().openTab(tab(a.sessionId, 'x'));
    await useSessionsStore.getState().close(id);
    expect(useSessionsStore.getState().sessionsByConnection[id]).toBeUndefined();
    expect(useTabsStore.getState().tabs.some((t) => t.payload?.sessionId === a.sessionId)).toBe(
      false,
    );
  });

  it('closing while a connection is opening discards the session once it arrives', async () => {
    const id = '00000000-0000-4000-8000-0000000000b1';
    const closeSpy = vi.spyOn(ipc, 'sessionClose');
    const opening = useSessionsStore.getState().open(id);
    const outcome = opening.then(
      () => 'opened',
      (e: { kind: string }) => e.kind,
    );
    await useSessionsStore.getState().close(id);
    expect(useSessionsStore.getState().opening[id]).toBeUndefined();
    expect(await outcome).toBe('cancelled');
    expect(useSessionsStore.getState().sessionsByConnection[id]).toBeUndefined();
    expect(closeSpy).toHaveBeenCalledTimes(1);
    // A later open works normally.
    const info = await useSessionsStore.getState().open(id);
    expect(useSessionsStore.getState().sessionsByConnection[id]).toEqual(info);
    await useSessionsStore.getState().close(id);
    closeSpy.mockRestore();
  });

  it('rejects with an IpcError for an unreachable host', async () => {
    await expect(
      useSessionsStore.getState().open('00000000-0000-4000-8000-0000000000ff'),
    ).rejects.toMatchObject({ kind: 'notFound' });
  });
});
