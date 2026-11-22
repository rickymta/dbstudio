import { beforeEach, describe, expect, it } from 'vitest';
import { getTabData } from '@/features/editor/editorStore';
import { addEditorTab } from '@/features/editor/open';
import { NO_SCOPE, useTabsStore, WELCOME_TAB } from '@/stores/tabs';
import { useWorkspaces } from './store';

beforeEach(() => {
  useTabsStore.setState({
    tabs: [WELCOME_TAB],
    activeId: WELCOME_TAB.id,
    scope: NO_SCOPE,
    stashed: {},
  });
  useWorkspaces.setState({ items: [], active: '', folder: '' });
});

describe('switching workspace', () => {
  it('keeps the editors of the workspace left behind (text and state), until they are closed', () => {
    const work = useWorkspaces.getState().create('Work', '#38bdf8', ['c1']);
    addEditorTab(
      { tabStateId: 'e1', connectionId: 'c1', sessionId: null, mode: 'sql' },
      { title: 'Query 1', text: 'select 1' },
    );
    useWorkspaces.getState().activate('');
    expect(useTabsStore.getState().tabs.map((t) => t.id)).toEqual([WELCOME_TAB.id]);
    expect(getTabData('e1')?.text).toBe('select 1'); // still alive while hidden

    useWorkspaces.getState().activate(work.id);
    expect(useTabsStore.getState().tabs.map((t) => t.id)).toContain('editor:e1');
    useTabsStore.getState().closeTab('editor:e1');
    expect(getTabData('e1')).toBeUndefined(); // closing for real still cleans up
  });

  it('a deleted workspace hands its tabs to "no workspace"', () => {
    const w = useWorkspaces.getState().create('Tmp', '#a78bfa', []);
    addEditorTab(
      { tabStateId: 'e2', connectionId: null, sessionId: null, mode: 'sql' },
      { title: 'Draft', text: 'x' },
    );
    useWorkspaces.getState().remove(w.id);
    expect(useWorkspaces.getState().active).toBe('');
    expect(useTabsStore.getState().tabs.map((t) => t.id)).toContain('editor:e2');
    expect(getTabData('e2')?.text).toBe('x');
  });
});
