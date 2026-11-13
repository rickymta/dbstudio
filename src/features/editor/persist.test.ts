import { beforeEach, describe, expect, it } from 'vitest';
import { ipc } from '@/lib/ipc';
import { editorTabId, useTabsStore } from '@/stores/tabs';
import { getTabData, patchTabData, resetEditorStore, useEditorStore } from './editorStore';
import { openEditor } from './open';
import { collectEditorTabs, restoreEditorTabs, startEditorPersistence } from './persist';
import { MYSQL_STAGING, PG_DEV, openSession, resetEditorWorld } from './testUtils';
import { resetEditorRunner } from './editorRunner';

beforeEach(resetEditorWorld);

/** Simulate a page reload: the UI stores are gone, the backend (mock store) stays. */
function reload() {
  useEditorStore.setState({ tabs: {} });
  resetEditorRunner();
  useTabsStore.setState({ tabs: useTabsStore.getState().tabs.slice(0, 1), activeId: 'welcome' });
  resetEditorStore();
}

describe('editor tab persistence', () => {
  it('saves the open tabs and restores them after a reload', async () => {
    const pg = await openSession(PG_DEV);
    const my = await openSession(MYSQL_STAGING);
    const a = openEditor(pg, {
      text: 'SELECT 1',
      path: {
        segments: [
          { kind: 'database', name: 'app_dev' },
          { kind: 'schema', name: 'audit' },
        ],
      },
    });
    const b = openEditor(my, { text: "SELECT 'é😀'" });
    const stop = startEditorPersistence();
    patchTabData(a, { cursor: 4, filePath: '/w/a.sql', autoCommit: false });
    patchTabData(b, { cursor: 99_999 });
    stop(); // flushes the pending write
    const saved = await ipc.editorTabsGet();
    expect(saved.map((s) => [s.id, s.position, s.text])).toEqual([
      [a, 0, 'SELECT 1'],
      [b, 1, "SELECT 'é😀'"],
    ]);
    expect(saved[0]).toMatchObject({
      connectionId: PG_DEV,
      database: 'app_dev',
      schema: 'audit',
      filePath: '/w/a.sql',
      cursor: 4,
      autoCommit: false,
    });

    reload();
    expect(useTabsStore.getState().tabs.map((t) => t.kind)).toEqual(['welcome']);
    expect(await restoreEditorTabs()).toBe(2);
    const tabs = useTabsStore.getState().tabs.filter((t) => t.kind === 'editor');
    expect(tabs.map((t) => t.id)).toEqual([editorTabId(a), editorTabId(b)]);
    // Restored tabs are detached from sessions, keep their text and the right mode.
    expect(tabs[0]!.payload).toMatchObject({ connectionId: PG_DEV, sessionId: null, mode: 'sql' });
    expect(getTabData(a)).toMatchObject({
      text: 'SELECT 1',
      cursor: 4,
      filePath: '/w/a.sql',
      savedText: 'SELECT 1',
      database: 'app_dev',
      schema: 'audit',
      autoCommit: false,
      leaseId: null,
    });
    // The cursor is clamped to the text.
    expect(getTabData(b)!.cursor).toBe("SELECT 'é😀'".length);
    // The active tab did not change.
    expect(useTabsStore.getState().activeId).toBe('welcome');
    // Restoring twice does not duplicate the tabs.
    expect(await restoreEditorTabs()).toBe(0);
  });

  it('writes again when a tab is closed', async () => {
    const pg = await openSession(PG_DEV);
    const a = openEditor(pg, { text: 'one' });
    const b = openEditor(pg, { text: 'two' });
    const stop = startEditorPersistence();
    await useTabsStore.getState().requestClose(editorTabId(a));
    stop();
    expect((await ipc.editorTabsGet()).map((s) => s.id)).toEqual([b]);
    expect(collectEditorTabs().map((s) => s.position)).toEqual([0]);
  });

  it('restores nothing when there is nothing saved', async () => {
    expect(await restoreEditorTabs()).toBe(0);
  });

  it('picks the console mode from the connection kind', async () => {
    const stored = [
      {
        id: 'e-redis',
        position: 0,
        title: 'R',
        connectionId: null,
        database: null,
        schema: null,
        text: 'GET k',
        filePath: null,
        cursor: 0,
        autoCommit: true,
      },
    ];
    await ipc.editorTabsSet(stored);
    expect(await restoreEditorTabs()).toBe(1);
    const tab = useTabsStore.getState().tabs.find((t) => t.id === editorTabId('e-redis'))!;
    if (tab.kind !== 'editor') throw new Error('not an editor');
    expect(tab.payload.mode).toBe('sql'); // unknown connection: default
  });
});
