import { beforeEach, describe, expect, it } from 'vitest';
import { openSession, resetEditorWorld, PG_DEV } from '@/features/editor/testUtils';
import { getTabData } from '@/features/editor/editorStore';
import { openEditorForActiveTab } from '@/features/editor/open';
import { useTabsStore, tableTabId } from '@/stores/tabs';
import type { ObjectPath } from '@/lib/types';
import { openSavedObject, savedObject, useNavigation } from './navigation';

const path: ObjectPath = {
  segments: [
    { kind: 'database', name: 'app_dev' },
    { kind: 'schema', name: 'public' },
    { kind: 'table', name: 'users' },
  ],
};

beforeEach(() => {
  resetEditorWorld();
  useNavigation.setState({ recent: [], favorites: [], request: null });
});

describe('workspace navigation', () => {
  it('reopens a saved object with the live session and retains its query context', async () => {
    const object = savedObject({
      id: 'old',
      kind: 'table',
      title: 'users',
      closable: true,
      payload: { connectionId: PG_DEV, sessionId: 'expired-session', path },
    })!;
    expect(openSavedObject(object)).toBe(false);
    const session = await openSession();
    expect(openSavedObject(object)).toBe(true);
    expect(useTabsStore.getState().activeId).toBe(tableTabId(session.sessionId, path));
    expect(openEditorForActiveTab()).toBe(true);
    const active = useTabsStore
      .getState()
      .tabs.find((t) => t.id === useTabsStore.getState().activeId)!;
    expect(active.kind).toBe('editor');
    if (active.kind !== 'editor') throw new Error('Expected query editor');
    expect(getTabData(active.payload.tabStateId)).toMatchObject({
      database: 'app_dev',
      schema: 'public',
    });
  });

  it('deduplicates recent objects across sessions and keeps favorites independent', () => {
    const tab = {
      id: 'table',
      kind: 'table' as const,
      title: 'users',
      closable: true,
      payload: { connectionId: PG_DEV, sessionId: 'session-one', path },
    };
    const first = savedObject(tab)!;
    const second = savedObject({ ...tab, payload: { ...tab.payload, sessionId: 'session-two' } })!;
    useNavigation.getState().visit(first);
    useNavigation.getState().toggle(first);
    useNavigation.getState().visit(second);
    expect(useNavigation.getState().recent).toHaveLength(1);
    expect(useNavigation.getState().favorites).toEqual([first]);
    useNavigation.getState().toggle(second);
    expect(useNavigation.getState().favorites).toEqual([]);
  });
});
