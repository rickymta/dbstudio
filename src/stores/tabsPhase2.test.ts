import { act, renderHook } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { openBackup } from '@/features/backup/open';
import { editorModeOf, openEditor } from '@/features/editor/open';
import { openErd } from '@/features/erd/open';
import type { SessionInfo } from '@/lib/types';
import {
  BACKUP_TAB_ID,
  WELCOME_TAB,
  editorTabId,
  tableTabId,
  useTabGuard,
  useTabsStore,
  type TabItem,
} from './tabs';

const path = { segments: [{ kind: 'table' as const, name: 't' }] };
const session = {
  sessionId: 's1',
  connectionId: 'c1',
  dialect: { name: 'postgres' },
} as SessionInfo;

beforeEach(() => useTabsStore.setState({ tabs: [WELCOME_TAB], activeId: WELCOME_TAB.id }));

describe('closeSessionTabs', () => {
  it('keeps editor tabs and only detaches their session', () => {
    const editor: TabItem = {
      id: editorTabId('e1'),
      kind: 'editor',
      title: 'Q',
      closable: true,
      payload: { tabStateId: 'e1', connectionId: 'c1', sessionId: 's1', mode: 'sql' },
    };
    const table: TabItem = {
      id: tableTabId('s1', path),
      kind: 'table',
      title: 't',
      closable: true,
      payload: { sessionId: 's1', connectionId: 'c1', path },
    };
    const other: TabItem = {
      ...table,
      id: 'other',
      payload: { ...table.payload!, sessionId: 's2' },
    };
    const { openTab, closeSessionTabs } = useTabsStore.getState();
    [editor, table, other].forEach(openTab);
    closeSessionTabs('s1');
    const { tabs } = useTabsStore.getState();
    expect(tabs.map((t) => t.id)).toEqual(['welcome', editor.id, 'other']);
    expect(tabs[1]).toMatchObject({
      kind: 'editor',
      payload: { tabStateId: 'e1', sessionId: null },
    });
  });

  it('only detaches when the session has nothing but editors', () => {
    openEditor(session);
    useTabsStore.getState().closeSessionTabs('s1');
    expect(useTabsStore.getState().tabs).toHaveLength(2);
    expect(useTabsStore.getState().activeId).toMatch(/^editor:/);
  });
});

describe('useTabGuard and requestClose', () => {
  it('closes unguarded tabs synchronously', async () => {
    openEditor(session);
    const id = useTabsStore.getState().activeId;
    const done = useTabsStore.getState().requestClose(id);
    expect(useTabsStore.getState().tabs.some((t) => t.id === id)).toBe(false);
    await expect(done).resolves.toBe(true);
  });

  it('lets a guard veto closing, using the latest guard', async () => {
    openEditor(session);
    const id = useTabsStore.getState().activeId;
    const first = vi.fn().mockResolvedValue(false);
    const second = vi.fn().mockResolvedValue(true);
    const { rerender, unmount } = renderHook(({ g }) => useTabGuard(id, g), {
      initialProps: { g: first },
    });
    await expect(useTabsStore.getState().requestClose(id)).resolves.toBe(false);
    expect(useTabsStore.getState().tabs.some((t) => t.id === id)).toBe(true);
    rerender({ g: second });
    await act(async () => {
      await expect(useTabsStore.getState().requestClose(id)).resolves.toBe(true);
    });
    expect(second).toHaveBeenCalledTimes(1);
    expect(useTabsStore.getState().tabs.some((t) => t.id === id)).toBe(false);
    unmount();
  });

  it('stops guarding after unmount', async () => {
    openEditor(session);
    const id = useTabsStore.getState().activeId;
    const { unmount } = renderHook(() => useTabGuard(id, () => false));
    unmount();
    await expect(useTabsStore.getState().requestClose(id)).resolves.toBe(true);
  });
});

describe('open helpers', () => {
  it('opens editor, erd and backup placeholder tabs', () => {
    openEditor(session);
    openErd(session, path);
    openErd(session, path); // same scope: only activated
    openBackup();
    openBackup();
    const tabs = useTabsStore.getState().tabs;
    expect(tabs.map((t) => t.kind)).toEqual(['welcome', 'editor', 'erd', 'backup']);
    expect(tabs[3]!.id).toBe(BACKUP_TAB_ID);
    expect(tabs[1]).toMatchObject({ payload: { sessionId: 's1', mode: 'sql' } });
  });

  it('picks the editor mode from the dialect', () => {
    const mode = (name: string) => editorModeOf({ dialect: { name } } as unknown as SessionInfo);
    expect(mode('mongoShell')).toBe('mongo');
    expect(mode('redisCli')).toBe('redis');
    expect(mode('mySql')).toBe('sql');
  });
});
