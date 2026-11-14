import { beforeEach, describe, expect, it } from 'vitest';
import { useTabsStore, type EditorMode } from '@/stores/tabs';
import { addEditorTab, appendToEditor, terminateStatements } from './open';
import { useEditorStore } from './editorStore';

beforeEach(() => {
  useEditorStore.setState({ tabs: {} });
  useTabsStore.setState({ tabs: [], activeId: 'welcome' });
});

function setup(text: string, mode: EditorMode = 'sql') {
  addEditorTab({ tabStateId: 'ed', connectionId: 'c1', sessionId: null, mode }, { text }, false);
}

describe('terminateStatements', () => {
  it('adds ; when the last statement has none', () => {
    expect(terminateStatements('select 1', 'sql')).toBe('select 1;');
    expect(terminateStatements('select 1  \n\n', 'sql')).toBe('select 1;  \n\n');
  });
  it('keeps text that already ends with ;', () => {
    expect(terminateStatements('select 1;', 'sql')).toBe('select 1;');
    expect(terminateStatements('select 1; -- done\n', 'sql')).toBe('select 1; -- done\n');
    expect(terminateStatements("select ';'; /* x */", 'sql')).toBe("select ';'; /* x */");
  });
  it('puts ; before trailing comments', () => {
    expect(terminateStatements('select 1 -- note', 'sql')).toBe('select 1; -- note');
    expect(terminateStatements('select 1 /* a ; b */\n-- c\n', 'sql')).toBe(
      'select 1; /* a ; b */\n-- c\n',
    );
  });
  it('is not fooled by ; inside strings', () => {
    expect(terminateStatements("select 'a;'", 'sql')).toBe("select 'a;';");
  });
  it('leaves empty, comment-only and unterminated text alone', () => {
    expect(terminateStatements('', 'sql')).toBe('');
    expect(terminateStatements('  \n-- only\n', 'sql')).toBe('  \n-- only\n');
    expect(terminateStatements("select 'abc", 'sql')).toBe("select 'abc");
  });
  it('mongo uses // comments; redis is never changed', () => {
    expect(terminateStatements('db.a.find() // x', 'mongo')).toBe('db.a.find(); // x');
    expect(terminateStatements('GET k', 'redis')).toBe('GET k');
  });
});

describe('appendToEditor', () => {
  it('terminates the previous statement and selects the new text', () => {
    setup('select * from t limit 20');
    expect(appendToEditor('ed', 'CREATE INDEX i ON t (a);')).toBe(true);
    const d = useEditorStore.getState().tabs.ed!;
    expect(d.text).toBe('select * from t limit 20;\n\nCREATE INDEX i ON t (a);');
    expect(d.text.slice(d.selection.from, d.selection.to)).toBe('CREATE INDEX i ON t (a);');
  });
  it('does not change a terminated statement or an empty editor', () => {
    setup('select 1;');
    appendToEditor('ed', 'X;');
    expect(useEditorStore.getState().tabs.ed!.text).toBe('select 1;\n\nX;');
    useEditorStore.setState({ tabs: {} });
    useTabsStore.setState({ tabs: [], activeId: 'welcome' });
    setup('');
    appendToEditor('ed', 'X;');
    expect(useEditorStore.getState().tabs.ed!.text).toBe('X;');
  });
  it('redis gets no ;', () => {
    setup('GET k', 'redis');
    appendToEditor('ed', 'SET a b');
    expect(useEditorStore.getState().tabs.ed!.text).toBe('GET k\n\nSET a b');
  });
});
