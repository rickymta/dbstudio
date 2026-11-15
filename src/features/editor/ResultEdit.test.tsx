// Editing the rows of a query result: Edit mode, the change set sent to the backend (keys and
// names by `columnMap`), the reasons shown for results that cannot be edited, read-only columns
// and the guard against running again with unsaved edits.
import { act, cleanup, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import { resetDataStores } from '@/features/data/store';
import { usePendingChanges } from '@/features/data/edit/pendingChanges';
import { polyfillLayoutApis } from '@/features/data/testUtils';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import type { ColumnMeta, ResultEditability, Value } from '@/lib/types';
import { runText } from './editorRunner';
import { useEditorStore, type RunSnapshot } from './editorStore';
import { ResultsPanel } from './ResultsPanel';
import { dropResultEdits, hasResultEdits, useResultEditStore } from './resultEdit';
import { ResultGridModel, type StatementResult } from './results';
import { resetEditorWorld } from './testUtils';

const TAB = 'tab-1';
const SESSION = 'session-1';
const LEASE = 'lease-1';
const GRID = `${TAB}:1:r0`;

const table = {
  segments: [
    { kind: 'database' as const, name: 'app' },
    { kind: 'table' as const, name: 'items' },
  ],
};

const col = (name: string, origin?: string): ColumnMeta => ({
  name,
  typeName: 'text',
  valueKind: name === 'id' ? 'int' : 'text',
  origin: origin ? { database: 'app', table: 'items', column: origin } : null,
});

// `SELECT id, name AS label, upper(name) AS shout FROM items`
const COLUMNS = [col('id', 'id'), col('label', 'name'), col('shout')];
const ROWS: Value[][] = [
  [
    { t: 'int', v: 1 },
    { t: 'text', v: 'a' },
    { t: 'text', v: 'A' },
  ],
  [
    { t: 'int', v: 2 },
    { t: 'text', v: 'b' },
    { t: 'text', v: 'B' },
  ],
];

function editable(over: Partial<ResultEditability> = {}): ResultEditability {
  return {
    editable: true,
    table,
    keyColumns: ['id'],
    columnMap: ['id', 'name', null],
    readOnlyColumns: [],
    reason: null,
    atomic: true,
    ...over,
  };
}

function runWith(columns: ColumnMeta[] = COLUMNS): RunSnapshot {
  const model = new ResultGridModel(SESSION);
  model.setColumns(columns);
  model.pushRows(ROWS.map((r) => r.slice(0, columns.length)));
  model.finish(false);
  const result: StatementResult = {
    key: 'r0',
    index: 0,
    range: { start: 0, end: 20 },
    messages: [],
    outcome: { kind: 'rows', rowCount: ROWS.length, truncated: false },
    elapsedMs: 1,
    error: null,
    model,
  };
  return {
    id: 1,
    status: 'ok',
    queryId: 'q',
    text: 'SELECT id, name AS label, upper(name) AS shout FROM items',
    baseChar: 0,
    results: [result],
    messages: [],
    error: null,
    errorMark: null,
    startedAt: 0,
    elapsedMs: 1,
    canFetchMore: false,
    fetchingMore: false,
    docAtStart: '',
  };
}

function renderPanel(run: RunSnapshot) {
  return render(
    <Providers>
      <ResultsPanel tabStateId={TAB} run={run} mode="sql" onLoadMore={() => {}} />
    </Providers>,
  );
}

const cell = (r: number, c: number) =>
  document.querySelector<HTMLElement>(`[role="gridcell"][data-r="${r}"][data-c="${c}"]`)!;

beforeAll(polyfillLayoutApis);
beforeEach(async () => {
  await i18n.changeLanguage('en');
  resetEditorWorld();
  useEditorStore.getState().ensure(TAB, {
    leaseSessionId: SESSION,
    leaseId: LEASE,
    autoCommit: true,
    leaseStatus: 'ready',
  });
});
afterEach(() => {
  cleanup();
  dropResultEdits(TAB);
  useResultEditStore.setState({ byGrid: {} });
  resetDataStores();
  vi.restoreAllMocks();
});

describe('editable result', () => {
  it('shows the Edit button, edits a cell and saves it with the table column names and key', async () => {
    const user = userEvent.setup();
    const check = vi.spyOn(ipc, 'resultEditability').mockResolvedValue(editable());
    const preview = vi.spyOn(ipc, 'changesPreview').mockResolvedValue(['UPDATE ...']);
    const apply = vi.spyOn(ipc, 'changesApply').mockResolvedValue({
      applied: 1,
      affected: [1],
      atomic: true,
    });
    renderPanel(runWith());

    const toggle = await screen.findByRole('button', { name: /Edit data/ });
    expect(check).toHaveBeenCalledWith(SESSION, LEASE, COLUMNS);
    await user.click(toggle);

    // `label` is `name` in the table; `shout` is an expression.
    await user.dblClick(cell(0, 1));
    const input = await screen.findByRole('textbox', { name: 'Value of label' });
    await user.clear(input);
    await user.type(input, 'zed{Enter}');
    expect(cell(0, 1)).toHaveAttribute('data-pending', 'updated');
    expect(cell(0, 1).textContent).toBe('zed');
    expect(screen.getByTestId('pending-count')).toHaveTextContent('1 pending change');

    const expected = {
      model: 'rows',
      table,
      keyColumns: ['id'],
      changes: [
        {
          op: 'update',
          key: [{ t: 'int', v: 1 }],
          set: [{ column: 'name', value: { kind: 'text', v: 'zed' } }],
        },
      ],
    };
    await user.click(screen.getByRole('button', { name: /Preview/ }));
    await waitFor(() => expect(preview).toHaveBeenCalledWith(SESSION, expected));
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: /Apply/ }));
    await waitFor(() => expect(apply).toHaveBeenCalledWith(SESSION, expected, LEASE));

    // Saved: the grid keeps the new value in place and nothing is pending any more.
    await waitFor(() => expect(hasResultEdits(TAB)).toBe(false));
    expect(cell(0, 1).textContent).toBe('zed');
    expect(cell(0, 1)).not.toHaveAttribute('data-pending');
    expect(screen.queryByTestId('result-needs-rerun')).toBeNull();
  });

  it('removes deleted rows from the grid and flags new rows for a re-run', async () => {
    const user = userEvent.setup();
    vi.spyOn(ipc, 'resultEditability').mockResolvedValue(editable());
    const apply = vi.spyOn(ipc, 'changesApply').mockResolvedValue({
      applied: 2,
      affected: [1, 1],
      atomic: true,
    });
    renderPanel(runWith());
    await user.click(await screen.findByRole('button', { name: /Edit data/ }));

    await user.click(cell(1, 1)); // select row 2
    await user.click(screen.getByRole('button', { name: 'Delete row' }));
    expect(cell(1, 1).closest('[role="row"]')).toHaveAttribute('data-pending', 'deleted');
    await user.click(screen.getByRole('button', { name: 'Add row' }));
    await user.click(screen.getByRole('button', { name: /^Apply$/ }));

    await waitFor(() => expect(apply).toHaveBeenCalled());
    const sent = apply.mock.calls[0]![1];
    expect(sent.model === 'rows' && sent.changes.map((c) => c.op)).toEqual(['delete', 'insert']);
    expect(await screen.findByTestId('result-needs-rerun')).toBeInTheDocument();
    expect(screen.getByText('1 row(s)')).toBeInTheDocument();
  });

  it('keeps expression columns read-only, with a tooltip saying why', async () => {
    const user = userEvent.setup();
    vi.spyOn(ipc, 'resultEditability').mockResolvedValue(editable());
    renderPanel(runWith());
    await user.click(await screen.findByRole('button', { name: /Edit data/ }));
    await user.dblClick(cell(0, 2));
    expect(screen.queryByRole('textbox', { name: 'Value of shout' })).toBeNull();
    expect(cell(0, 2)).toHaveAttribute('title', expect.stringMatching(/expression/i));
    expect(cell(0, 2)).not.toHaveAttribute('data-pending');
  });
});

describe('run again', () => {
  it('runs only the statement that produced the result, not the rest of the script', async () => {
    const user = userEvent.setup();
    vi.spyOn(ipc, 'resultEditability').mockResolvedValue(editable());
    const exec = vi
      .spyOn(ipc, 'leaseExecute')
      .mockRejectedValue({ kind: 'connection', message: 'no server' });
    const run = runWith();
    const select = run.text;
    run.text = `DELETE FROM items; ${select}`;
    run.results[0]!.range = { start: 19, end: 19 + select.length };
    renderPanel(run);
    await user.click(await screen.findByRole('button', { name: /Run query again/ }));
    await waitFor(() => expect(exec).toHaveBeenCalled());
    expect(exec.mock.calls[0]![1].text).toBe(select);
  });
});

describe('result that cannot be edited', () => {
  it.each([
    [
      { editable: false, reason: 'multipleTables' as const, table: null },
      'Result from several tables',
    ],
    [
      {
        editable: false,
        reason: 'keyNotInResult' as const,
        missingKeyColumns: ['id'],
        columnMap: [null, 'name', null],
      },
      'Key column id is missing from the SELECT',
    ],
    [{ editable: false, reason: 'noUniqueKey' as const }, 'no primary key'],
    [{ editable: false, reason: 'readOnlyConnection' as const }, 'read-only'],
  ])('says why: %o', async (over, text) => {
    vi.spyOn(ipc, 'resultEditability').mockResolvedValue(editable(over));
    renderPanel(runWith());
    const reason = await screen.findByTestId('result-readonly-reason');
    expect(reason).toHaveTextContent(text);
    expect(reason).toHaveTextContent(/^Read-only: /);
    expect(screen.queryByRole('button', { name: /Edit data/ })).toBeNull();
  });

  it('does not ask the backend when no column names its table', async () => {
    const check = vi.spyOn(ipc, 'resultEditability');
    renderPanel(runWith([col('id'), col('label'), col('shout')]));
    await screen.findByTestId('result-grid');
    expect(check).not.toHaveBeenCalled();
    expect(screen.queryByTestId('result-readonly-reason')).toBeNull();
  });

  it('refuses a result with two columns of the same name', async () => {
    vi.spyOn(ipc, 'resultEditability').mockResolvedValue(
      editable({ columnMap: ['id', 'name', null] }),
    );
    renderPanel(runWith([col('id', 'id'), col('id', 'name'), col('shout')]));
    expect(await screen.findByTestId('result-readonly-reason')).toHaveTextContent(
      'two columns with the same name',
    );
  });
});

describe('unsaved edits', () => {
  it('block running again until the user agrees, then drop the edits', async () => {
    const leaseExecute = vi.spyOn(ipc, 'leaseExecute');
    usePendingChanges.getState().addInsert(GRID, {});
    expect(hasResultEdits(TAB)).toBe(true);

    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(false);
    const kept = await runText(TAB, SESSION, { text: 'SELECT 1', baseChar: 0 });
    expect(kept).toBeNull();
    expect(confirm).toHaveBeenCalledTimes(1);
    expect(leaseExecute).not.toHaveBeenCalled();
    expect(hasResultEdits(TAB)).toBe(true);

    confirm.mockReturnValue(true);
    act(() => {
      leaseExecute.mockRejectedValue({ kind: 'connection', message: 'no server' });
    });
    await runText(TAB, SESSION, { text: 'SELECT 1', baseChar: 0 });
    expect(hasResultEdits(TAB)).toBe(false);
  });
});
