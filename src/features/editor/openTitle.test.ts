import { beforeEach, describe, expect, it } from 'vitest';
import i18n from '@/lib/i18n';
import { useTabsStore } from '@/stores/tabs';
import { addEditorTab, nextEditorTitle } from './open';
import { useEditorStore } from './editorStore';

beforeEach(async () => {
  await i18n.changeLanguage('en');
  useEditorStore.setState({ tabs: {} });
  useTabsStore.setState({ tabs: [], activeId: 'welcome' });
});

describe('nextEditorTitle', () => {
  it('skips the titles of open editors, restored ones included', () => {
    // A restored tab keeps its saved title while the counter restarted at the reload.
    for (const n of [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20]) {
      addEditorTab(
        { tabStateId: `ed${n}`, connectionId: 'c1', sessionId: null, mode: 'sql' },
        { title: `Query ${n}` },
        false,
      );
    }
    const first = nextEditorTitle();
    const second = nextEditorTitle();
    expect(first).toBe('Query 21');
    expect(second).toBe('Query 22');
  });

  it('never repeats a title within a run', () => {
    const a = nextEditorTitle();
    addEditorTab(
      { tabStateId: 'x', connectionId: 'c1', sessionId: null, mode: 'sql' },
      { title: a },
      false,
    );
    expect(nextEditorTitle()).not.toBe(a);
  });
});
