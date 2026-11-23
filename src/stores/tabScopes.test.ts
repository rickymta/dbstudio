import { beforeEach, describe, expect, it } from 'vitest';
import { allTabs, NO_SCOPE, scopeOfTab, useTabsStore, WELCOME_TAB, type TabItem } from './tabs';

const table = (id: string, sessionId = 's1'): TabItem => ({
  id,
  kind: 'table',
  title: id,
  closable: true,
  payload: { sessionId, connectionId: 'c1', path: { segments: [{ kind: 'table', name: id }] } },
});
const editor = (id: string, sessionId: string | null = 's1'): TabItem => ({
  id: `editor:${id}`,
  kind: 'editor',
  title: id,
  closable: true,
  payload: { tabStateId: id, connectionId: 'c1', sessionId, mode: 'sql' },
});

beforeEach(() => {
  useTabsStore.setState({
    tabs: [WELCOME_TAB],
    activeId: WELCOME_TAB.id,
    scope: NO_SCOPE,
    stashed: {},
  });
});

describe('tab sets per workspace', () => {
  it('activates a transferred tab when a deleted scope joins an empty scope', () => {
    useTabsStore.getState().closeTab(WELCOME_TAB.id);
    useTabsStore.getState().switchScope('w1');
    useTabsStore.getState().closeTab(WELCOME_TAB.id);
    useTabsStore.getState().openTab(table('t1'));
    useTabsStore.getState().switchScope(NO_SCOPE);
    useTabsStore.getState().dropScope('w1');
    expect(useTabsStore.getState().activeId).toBe('t1');
  });

  it('keeps Welcome closed when revisiting a scope during the same app session', () => {
    useTabsStore.getState().closeTab(WELCOME_TAB.id);
    useTabsStore.getState().switchScope('w1');
    expect(useTabsStore.getState().tabs).toEqual([WELCOME_TAB]);
    useTabsStore.getState().switchScope(NO_SCOPE);
    expect(useTabsStore.getState().tabs).toEqual([]);
    expect(useTabsStore.getState().activeId).toBe('');
  });

  it('keeps each scope its own tabs and active tab, and every tab stays alive', () => {
    const s = useTabsStore.getState();
    s.openTab(table('t1'));
    s.switchScope('w1');
    expect(useTabsStore.getState().tabs.map((t) => t.id)).toEqual([WELCOME_TAB.id]);
    useTabsStore.getState().openTab(editor('e1'));
    expect(
      allTabs(useTabsStore.getState())
        .map((t) => t.id)
        .sort(),
    ).toEqual(['editor:e1', 't1', WELCOME_TAB.id, WELCOME_TAB.id].sort());
    useTabsStore.getState().switchScope(NO_SCOPE);
    expect(useTabsStore.getState().activeId).toBe('t1');
    expect(scopeOfTab(useTabsStore.getState(), 'editor:e1')).toBe('w1');
  });

  it('closes the tabs of a closed session in every scope and detaches editors', () => {
    const s = useTabsStore.getState();
    s.openTab(table('t1'));
    s.switchScope('w1');
    useTabsStore.getState().openTab(table('t2'));
    useTabsStore.getState().openTab(editor('e1'));
    useTabsStore.getState().switchScope(NO_SCOPE);
    useTabsStore.getState().closeSessionTabs('s1');
    const state = useTabsStore.getState();
    expect(state.tabs.map((t) => t.id)).toEqual([WELCOME_TAB.id]);
    const hidden = state.stashed.w1!.tabs;
    expect(hidden.map((t) => t.id)).toEqual([WELCOME_TAB.id, 'editor:e1']);
    expect(hidden[1]!.payload!.sessionId).toBeNull();
    // Its active tab (the editor) survives the session, so it stays the active one.
    expect(state.stashed.w1!.activeId).toBe('editor:e1');
  });

  it('gives the tabs of a deleted workspace to "no workspace"', () => {
    useTabsStore.getState().switchScope('w1');
    useTabsStore.getState().openTab(editor('e1'));
    useTabsStore.getState().dropScope('w1');
    const state = useTabsStore.getState();
    expect(state.scope).toBe(NO_SCOPE);
    expect(state.tabs.map((t) => t.id)).toEqual([WELCOME_TAB.id, 'editor:e1']);

    useTabsStore.getState().switchScope('w2');
    useTabsStore.getState().openTab(table('t9'));
    useTabsStore.getState().switchScope('w3');
    useTabsStore.getState().dropScope('w2');
    expect(useTabsStore.getState().stashed[NO_SCOPE]!.tabs.map((t) => t.id)).toContain('t9');
  });

  it('moves restored tabs into the set of their workspace', () => {
    useTabsStore.getState().openTab(editor('e1'));
    useTabsStore.getState().openTab(editor('e2'));
    useTabsStore.getState().moveToScope(['editor:e2'], 'w1');
    const state = useTabsStore.getState();
    expect(state.tabs.map((t) => t.id)).toEqual([WELCOME_TAB.id, 'editor:e1']);
    expect(state.stashed.w1!.tabs.map((t) => t.id)).toEqual([WELCOME_TAB.id, 'editor:e2']);
  });
});
