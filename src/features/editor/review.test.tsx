// Regression tests for the issues found in review of package C.
import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { createRef } from 'react';
import { beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import { polyfillLayoutApis } from '@/features/data/testUtils';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { mockBreakLease } from '@/lib/mock/leases';
import { mockRecordHistory } from '@/lib/mock/history';
import { editorTabId, useTabsStore } from '@/stores/tabs';
import { EditorHost } from './EditorHost';
import { EditorTab } from './EditorTab';
import {
  applyLeaseState,
  cancelRun,
  endTransaction,
  fetchMore,
  runText,
  setAutoCommit,
} from './editorRunner';
import {
  getTabData,
  hasOpenTransaction,
  isDirty,
  patchTabData,
  type RunSnapshot,
} from './editorStore';
import { openEditor } from './open';
import {
  collectEditorTabs,
  resetRestoreOnce,
  restoreEditorTabs,
  startEditorPersistence,
} from './persist';
import { ResultsPanel } from './ResultsPanel';
import { HistoryList } from './SidePanel';
import { SqlEditor, type SqlEditorHandle } from './SqlEditor';
import { openSession, resetEditorWorld } from './testUtils';
import type { StatementResult } from './results';

beforeAll(polyfillLayoutApis);
beforeEach(async () => {
  await i18n.changeLanguage('en');
  resetEditorWorld();
  resetRestoreOnce();
  localStorage.clear();
});

async function editorWithTx() {
  const session = await openSession();
  const id = openEditor(session, { text: 'BEGIN' });
  await runText(id, session.sessionId, { text: 'BEGIN', baseChar: 0 });
  expect(getTabData(id)!.leaseState!.transaction).toBe('active');
  return { session, id };
}

describe('HIGH 1: a lost connection never commits on a fresh lease', () => {
  it('commit / rollback / auto-commit report the lost transaction and open no new lease', async () => {
    const { session, id } = await editorWithTx();
    const leaseId = getTabData(id)!.leaseId!;
    mockBreakLease(leaseId);
    applyLeaseState(leaseId, await ipc.leaseState(leaseId));
    expect(getTabData(id)!.transactionLost).toBe(true);
    expect(hasOpenTransaction(getTabData(id))).toBe(false);

    const open = vi.spyOn(ipc, 'leaseOpen');
    expect(await endTransaction(id, session.sessionId, 'commit')).toBe(false);
    expect(await endTransaction(id, session.sessionId, 'rollback')).toBe(false);
    expect(open).not.toHaveBeenCalled();
    expect(getTabData(id)!.leaseId).toBe(leaseId);
    // Only the preference changes; the next run opens a new lease.
    expect(await setAutoCommit(id, session.sessionId, false)).toBe('ok');
    expect(open).not.toHaveBeenCalled();
    // Closing the tab does not ask: the transaction is already gone.
    expect(await useTabsStore.getState().requestClose(editorTabId(id))).toBe(true);
  });
});

describe('HIGH 2: shortcuts and the close dialog', () => {
  it('queues close requests; none hangs', async () => {
    const user = userEvent.setup();
    const { id } = await editorWithTx();
    render(
      <Providers>
        <EditorHost />
      </Providers>,
    );
    let first: Promise<boolean> | undefined;
    let second: Promise<boolean> | undefined;
    act(() => {
      first = useTabsStore.getState().requestClose(editorTabId(id));
      second = useTabsStore.getState().requestClose(editorTabId(id));
    });
    let dialog = await screen.findByRole('dialog', { name: 'A transaction is still open' });
    await user.click(within(dialog).getByRole('button', { name: 'Keep tab open' }));
    expect(await first).toBe(false);
    dialog = await screen.findByRole('dialog', { name: 'A transaction is still open' });
    await user.click(within(dialog).getByRole('button', { name: 'Roll back and close' }));
    expect(await second).toBe(true);
  });

  it('Ctrl+W closes once: ignored when already handled or when a dialog is open', async () => {
    const session = await openSession();
    const id = openEditor(session, { text: '' });
    render(
      <Providers>
        <EditorHost />
      </Providers>,
    );
    const press = (prevent: boolean) => {
      const ev = new KeyboardEvent('keydown', {
        key: 'w',
        ctrlKey: true,
        bubbles: true,
        cancelable: true,
      });
      if (prevent) ev.preventDefault();
      window.dispatchEvent(ev);
    };
    press(true); // the tab bar already handled it
    expect(useTabsStore.getState().tabs.some((t) => t.id === editorTabId(id))).toBe(true);
    press(false);
    await waitFor(() =>
      expect(useTabsStore.getState().tabs.some((t) => t.id === editorTabId(id))).toBe(false),
    );
  });

  it('Mod-/ in the editor only toggles a comment and does not open the cheat-sheet', async () => {
    const session = await openSession();
    const id = openEditor(session, { text: 'SELECT 1' });
    const tab = useTabsStore.getState().tabs.find((t) => t.id === editorTabId(id))!;
    if (tab.kind !== 'editor') throw new Error('not an editor');
    render(
      <Providers>
        <EditorTab {...tab.payload} />
        <EditorHost />
      </Providers>,
    );
    const content = (
      await screen.findByTestId('sql-editor', undefined, { timeout: 5000 })
    ).querySelector('.cm-content')!;
    content.dispatchEvent(
      new KeyboardEvent('keydown', { key: '/', ctrlKey: true, bubbles: true, cancelable: true }),
    );
    expect(screen.queryByRole('dialog', { name: 'Keyboard shortcuts' })).toBeNull();
    // Outside the editor it opens.
    window.dispatchEvent(
      new KeyboardEvent('keydown', { key: '/', ctrlKey: true, bubbles: true, cancelable: true }),
    );
    expect(await screen.findByRole('dialog', { name: 'Keyboard shortcuts' })).toBeInTheDocument();
  });
});

describe('MEDIUM 3: saving without a write grant', () => {
  it('falls back to Save As and keeps the text', async () => {
    const user = userEvent.setup();
    const session = await openSession();
    const id = openEditor(session, { text: 'SELECT 7' });
    patchTabData(id, { filePath: '/old/q.sql', savedText: '' });
    const tab = useTabsStore.getState().tabs.find((t) => t.id === editorTabId(id))!;
    if (tab.kind !== 'editor') throw new Error('not an editor');
    const save = vi
      .spyOn(ipc, 'sqlFileSave')
      .mockRejectedValueOnce({ kind: 'permission', message: 'no grant' })
      .mockResolvedValueOnce('/new/q.sql');
    render(
      <Providers>
        <EditorTab {...tab.payload} />
      </Providers>,
    );
    await screen.findByTestId('sql-editor', undefined, { timeout: 5000 });
    await user.click(screen.getByRole('button', { name: /Save .sql file/ }));
    await waitFor(() => expect(getTabData(id)!.filePath).toBe('/new/q.sql'));
    expect(save).toHaveBeenNthCalledWith(1, '/old/q.sql', 'SELECT 7');
    expect(save).toHaveBeenNthCalledWith(2, null, 'SELECT 7');
    expect(getTabData(id)!.text).toBe('SELECT 7');
    expect(isDirty(getTabData(id)!)).toBe(false);
  });
});

describe('MEDIUM 4/5/7: persistence', () => {
  it('keeps the unsaved mark of file-backed tabs across a reload', async () => {
    const session = await openSession();
    const a = openEditor(session, { text: 'one' });
    const b = openEditor(session, { text: 'two' });
    patchTabData(a, { filePath: '/a.sql', savedText: 'one' });
    patchTabData(b, { filePath: '/b.sql', savedText: 'two' });
    const stop = startEditorPersistence();
    patchTabData(b, { text: 'two edited' });
    stop();
    useTabsStore.setState({ tabs: useTabsStore.getState().tabs.slice(0, 1), activeId: 'welcome' });
    expect(await restoreEditorTabs()).toBe(2);
    expect(isDirty(getTabData(a)!)).toBe(false);
    expect(isDirty(getTabData(b)!)).toBe(true);
  });

  it('a failed read of the saved tabs reports null (no saving must start); a failed connection list does not block', async () => {
    const get = vi
      .spyOn(ipc, 'editorTabsGet')
      .mockRejectedValueOnce({ kind: 'storage', message: 'x' });
    expect(await restoreEditorTabs()).toBeNull();
    get.mockRestore();
    await ipc.editorTabsSet([
      {
        id: 'e1',
        position: 0,
        title: 'T',
        connectionId: 'c',
        database: null,
        schema: null,
        text: 'x',
        filePath: null,
        cursor: 0,
        autoCommit: true,
      },
    ]);
    vi.spyOn(ipc, 'connectionsList').mockRejectedValueOnce({ kind: 'storage', message: 'x' });
    expect(await restoreEditorTabs()).toBe(1);
    expect(collectEditorTabs()[0]!.id).toBe('e1');
  });

  it('does not write when only run results change', async () => {
    const session = await openSession();
    const id = openEditor(session, { text: 'x' });
    const set = vi.spyOn(ipc, 'editorTabsSet');
    const stop = startEditorPersistence();
    await runText(id, session.sessionId, { text: 'SELECT 1', baseChar: 0 });
    await new Promise((r) => setTimeout(r, 650));
    expect(set).not.toHaveBeenCalled();
    patchTabData(id, { text: 'changed' });
    await waitFor(() => expect(set).toHaveBeenCalledTimes(1), { timeout: 2000 });
    stop();
  });
});

describe('MEDIUM 7: debounced editor text', () => {
  it('reports typing after a pause, or at once through flush', async () => {
    const onChange = vi.fn();
    const ref = createRef<SqlEditorHandle>();
    render(
      <SqlEditor
        ref={ref}
        initialText=""
        initialSelection={{ from: 0, to: 0 }}
        mode="sql"
        dialect={{ name: 'postgres' }}
        target={{
          sessionId: null,
          mode: 'sql',
          bracketQuotes: false,
          database: null,
          schema: null,
        }}
        marks={[]}
        ariaLabel="q"
        onShortcut={() => {}}
        onChange={onChange}
        onSelection={() => {}}
      />,
    );
    act(() => ref.current!.replaceDoc('abc'));
    expect(onChange).not.toHaveBeenCalled();
    act(() => ref.current!.flush());
    expect(onChange).toHaveBeenCalledWith('abc');
    act(() => ref.current!.replaceDoc('abcd'));
    await waitFor(() => expect(onChange).toHaveBeenLastCalledWith('abcd'));
  });
});

function fakeRun(count: number): RunSnapshot {
  const results: StatementResult[] = Array.from({ length: count }, (_, i) => ({
    key: `r${i}`,
    index: i,
    range: { start: 0, end: 8 },
    messages: [],
    outcome: { kind: 'done' },
    elapsedMs: 1,
    error: null,
    model: null,
  }));
  return {
    id: 1,
    status: 'ok',
    queryId: 'q',
    text: 'SELECT 1',
    baseChar: 0,
    results,
    messages: [],
    error: null,
    errorMark: null,
    startedAt: 0,
    elapsedMs: 1,
    canFetchMore: false,
    fetchingMore: false,
    docAtStart: 'SELECT 1',
  };
}

describe('MEDIUM 6: many result tabs', () => {
  it('draws at most 50 result tabs and says how many are hidden', () => {
    render(
      <Providers>
        <ResultsPanel tabStateId="t" run={fakeRun(120)} onLoadMore={() => {}} />
      </Providers>,
    );
    const tabs = within(screen.getByRole('tablist')).getAllByRole('tab');
    expect(tabs).toHaveLength(50);
    expect(screen.getByTestId('results-hidden')).toHaveTextContent('70 more results');
    expect(tabs[0]).toHaveTextContent('#1 SELECT');
  });
});

describe('MEDIUM 8: history entries', () => {
  it('a click inserts once and never opens a tab; opening a tab is a separate button', async () => {
    const user = userEvent.setup();
    mockRecordHistory({ connectionId: 'c', database: null, text: 'SELECT 99', kind: 'query' });
    const onInsert = vi.fn();
    const onOpen = vi.fn();
    render(
      <Providers>
        <HistoryList
          connectionId={null}
          dbKind={null}
          currentText={() => ''}
          runId={null}
          onInsert={onInsert}
          onOpenInNewTab={onOpen}
        />
      </Providers>,
    );
    const entry = await screen.findByTestId('history-entry');
    await user.dblClick(within(entry).getByRole('button', { name: /SELECT 99/ }));
    expect(onOpen).not.toHaveBeenCalled();
    expect(onInsert).toHaveBeenCalledTimes(2); // two deliberate clicks, nothing else
    await user.click(within(entry).getByRole('button', { name: 'Open in a new tab' }));
    expect(onOpen).toHaveBeenCalledWith('SELECT 99');
  });
});

describe('MEDIUM 9: fetching more counts as running', () => {
  it('blocks a new run and can be cancelled', async () => {
    const session = await openSession();
    const id = openEditor(session, { text: '' });
    await runText(id, session.sessionId, {
      text: 'SELECT * FROM generate_series(1, 2500)',
      baseChar: 0,
    });
    expect(getTabData(id)!.run!.canFetchMore).toBe(true);
    const cancel = vi.spyOn(ipc, 'leaseCancel');
    const more = fetchMore(id);
    expect(getTabData(id)!.run!.fetchingMore).toBe(true);
    expect(await runText(id, session.sessionId, { text: 'SELECT 1', baseChar: 0 })).toBeNull();
    await cancelRun(id);
    expect(cancel).toHaveBeenCalledWith(getTabData(id)!.leaseId, getTabData(id)!.run!.queryId);
    await more;
    expect(getTabData(id)!.run!.fetchingMore).toBe(false);
  });
});

describe('LOW 10: error marks use the text that ran', () => {
  it('locates the error in the document of the run even if it was edited meanwhile', async () => {
    const session = await openSession();
    const doc = 'SELECT pg_sleep(0.1), nope FROM users';
    const id = openEditor(session, { text: doc });
    const running = runText(id, session.sessionId, { text: doc, baseChar: 0 });
    await new Promise((r) => setTimeout(r, 20));
    patchTabData(id, { text: `-- edited\n${doc}` });
    const snap = (await running)!;
    expect(doc.slice(snap.errorMark!.from, snap.errorMark!.to)).toBe('nope');
    expect(snap.docAtStart).toBe(doc);
  });
});
