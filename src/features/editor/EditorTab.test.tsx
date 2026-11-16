import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { polyfillLayoutApis } from '@/features/data/testUtils';
import { Providers } from '@/app/providers';
import { StatusBar } from '@/app/StatusBar';
import { Toaster } from '@/components/ui/Toast';
import { useToastStore } from '@/components/ui/toastStore';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { mockSetSqlFileToOpen } from '@/lib/mock/history';
import { mockBreakLease } from '@/lib/mock/leases';
import { editorTabId, useTabsStore } from '@/stores/tabs';
import { ConfirmDestructiveHost } from '@/features/safety/ConfirmDestructiveHost';
import { EditorHost } from './EditorHost';
import { resetRestoreOnce } from './persist';
import { EditorTab } from './EditorTab';
import { getTabData, patchTabData } from './editorStore';
import { usePlanStore } from '@/features/plan/planStore';
import { openEditor } from './open';
import { addProductionProfile, openSession, resetEditorWorld } from './testUtils';

beforeAll(polyfillLayoutApis);
beforeEach(async () => {
  await i18n.changeLanguage('en');
  resetEditorWorld();
  resetRestoreOnce();
});

async function mountEditor(text = 'SELECT * FROM users', connectionId?: string) {
  const session = await openSession(connectionId);
  const id = openEditor(session, { text });
  const tab = useTabsStore.getState().tabs.find((t) => t.id === editorTabId(id))!;
  if (tab.kind !== 'editor') throw new Error('not an editor tab');
  const view = render(
    <Providers>
      <EditorTab {...tab.payload} />
      <EditorHost />
      <ConfirmDestructiveHost />
    </Providers>,
  );
  await screen.findByTestId('sql-editor', undefined, { timeout: 5000 });
  return { id, session, ...view };
}

const run = () =>
  userEvent.click(screen.getByRole('button', { name: /^Run (statement|selection)$/ }));

describe('EditorTab', () => {
  it('runs the statement at the cursor and shows one result tab with the grid', async () => {
    const { id } = await mountEditor('SELECT * FROM users');
    await run();
    const tabs = await screen.findByRole('tablist', { name: 'Results' });
    expect(await within(tabs).findByRole('tab', { name: /#1 SELECT/ })).toBeInTheDocument();
    await waitFor(() => expect(screen.getByTestId('result-grid')).toBeInTheDocument());
    await waitFor(() => expect(screen.getByText('1,000 row(s)')).toBeInTheDocument());
    expect(screen.getByRole('button', { name: 'Load more' })).toBeInTheDocument();
    expect(getTabData(id)!.run!.status).toBe('ok');
  });

  it('shows several result sets, affected rows and server messages', async () => {
    await mountEditor("SELECT 1; UPDATE users SET a = 1 WHERE id = 1; DO $$ RAISE NOTICE 'hi' $$");
    // Run the whole script.
    await userEvent.click(screen.getByRole('button', { name: /Run script/ }));
    const tabs = await screen.findByRole('tablist', { name: 'Results' });
    await waitFor(() => expect(within(tabs).getAllByRole('tab')).toHaveLength(4)); // 3 + Messages
    await userEvent.click(within(tabs).getByRole('tab', { name: /#2 UPDATE/ }));
    expect(await screen.findByText('3 row(s) affected')).toBeInTheDocument();
    await userEvent.click(within(tabs).getByRole('tab', { name: 'Messages' }));
    expect(await screen.findByText(/NOTICE:\s+hi/)).toBeInTheDocument();
  });

  it('reports the error with its line and column', async () => {
    const { id } = await mountEditor("SELECT '😀', nope FROM users");
    await run();
    const alert = await screen.findByTestId('result-error');
    expect(alert).toHaveTextContent('column "nope" does not exist');
    expect(alert).toHaveTextContent('Line 1, column 13');
    const mark = getTabData(id)!.run!.errorMark!;
    expect(getTabData(id)!.text.slice(mark.from, mark.to)).toBe('nope');
  });

  it('shows server details after the first line of an error as a separate block', async () => {
    await mountEditor('SELECT failValidation');
    await run();
    const alert = await screen.findByTestId('result-error');
    expect(within(alert).getByText('Document failed validation (code 121)')).toBeInTheDocument();
    const detail = within(alert).getByTestId('result-error-detail');
    expect(detail.textContent).toMatch(/^errInfo: \{\n {2}"failingDocumentId": 1,/);
    expect(detail.textContent).toContain('"operatorName": "$jsonSchema"');
  });

  it('asks for parameter values first and validates numbers', async () => {
    const user = userEvent.setup();
    await mountEditor('SELECT * FROM users WHERE id = :id');
    await run();
    const dialog = await screen.findByRole('dialog', { name: 'Query parameters' });
    const runButton = within(dialog).getByRole('button', { name: 'Run' });
    expect(within(dialog).getByText(':id')).toBeInTheDocument();
    // Switch the type to Number and type something invalid.
    await user.click(within(dialog).getByRole('combobox', { name: 'Type of :id' }));
    await user.click(await screen.findByRole('option', { name: 'Number' }));
    const input = within(dialog).getByRole('textbox', { name: 'Value of :id' });
    await user.type(input, '12x');
    expect(within(dialog).getByText('Not a valid number.')).toBeInTheDocument();
    expect(runButton).toBeDisabled();
    await user.clear(input);
    await user.type(input, '-12.5e3');
    expect(runButton).toBeEnabled();
    await user.click(runButton);
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    await screen.findByTestId('result-grid');
  });

  it('runs on Ctrl+Enter inside the editor', async () => {
    const { id } = await mountEditor('SELECT 1');
    const content = screen.getByTestId('sql-editor').querySelector('.cm-content')!;
    content.dispatchEvent(
      new KeyboardEvent('keydown', {
        key: 'Enter',
        ctrlKey: true,
        bubbles: true,
        cancelable: true,
      }),
    );
    await waitFor(() => expect(getTabData(id)!.run?.status).toBe('ok'));
  });

  it('formats the SQL with the dialect of the connection', async () => {
    const { id } = await mountEditor('select a,b from t where x=1');
    await userEvent.click(screen.getByRole('button', { name: /Format SQL/ }));
    await waitFor(() => expect(getTabData(id)!.text).toContain('SELECT\n  a,\n  b\nFROM\n  t'));
  });

  it('switches to manual transactions: commit and rollback buttons follow the state', async () => {
    const user = userEvent.setup();
    const { id } = await mountEditor('UPDATE users SET a = 1 WHERE id = 1');
    const commit = screen.getByRole('button', { name: /Commit/ });
    expect(commit).toBeDisabled();
    await user.click(screen.getByRole('switch', { name: 'Auto-commit' }));
    await run();
    await waitFor(() =>
      expect(screen.getByTestId('tx-state')).toHaveAttribute('data-tx', 'active'),
    );
    expect(screen.getByTestId('tx-state')).toHaveTextContent('Transaction open');
    expect(commit).toBeEnabled();
    await user.click(commit);
    await waitFor(() => expect(screen.getByTestId('tx-state')).toHaveAttribute('data-tx', 'idle'));
    expect(getTabData(id)!.leaseState!.autoCommit).toBe(false);
  });

  it('asks before turning auto-commit on while a transaction is open', async () => {
    const user = userEvent.setup();
    const { id } = await mountEditor('BEGIN');
    await run();
    await waitFor(() => expect(getTabData(id)!.leaseState?.transaction).toBe('active'));
    await user.click(screen.getByRole('switch', { name: 'Auto-commit' }));
    // Auto-commit was already on: the switch turned it off, which is fine; turn it on again.
    await user.click(screen.getByRole('switch', { name: 'Auto-commit' }));
    const dialog = await screen.findByRole('dialog', { name: 'A transaction is open' });
    await user.click(within(dialog).getByRole('button', { name: 'Roll back and switch' }));
    await waitFor(() => expect(getTabData(id)!.leaseState!.transaction).toBe('idle'));
    await waitFor(() => expect(getTabData(id)!.leaseState!.autoCommit).toBe(true));
  });

  it('asks before closing a tab with an open transaction', async () => {
    const user = userEvent.setup();
    const { id } = await mountEditor('BEGIN');
    await run();
    await waitFor(() => expect(getTabData(id)!.leaseState?.transaction).toBe('active'));
    let closed: Promise<boolean> | undefined;
    act(() => {
      closed = useTabsStore.getState().requestClose(editorTabId(id));
    });
    const dialog = await screen.findByRole('dialog', { name: 'A transaction is still open' });
    await user.click(within(dialog).getByRole('button', { name: 'Keep tab open' }));
    expect(await closed).toBe(false);
    expect(useTabsStore.getState().tabs.some((t) => t.id === editorTabId(id))).toBe(true);

    act(() => {
      closed = useTabsStore.getState().requestClose(editorTabId(id));
    });
    const again = await screen.findByRole('dialog', { name: 'A transaction is still open' });
    await user.click(within(again).getByRole('button', { name: 'Roll back and close' }));
    expect(await closed).toBe(true);
    expect(useTabsStore.getState().tabs.some((t) => t.id === editorTabId(id))).toBe(false);
  });

  it('asks for the database name before destructive statements on production', async () => {
    const user = userEvent.setup();
    const prod = await addProductionProfile();
    const { id } = await mountEditor('DROP TABLE users', prod.id);
    await run();
    const dialog = await screen.findByRole('dialog', { name: 'Confirm destructive statements' });
    const confirm = within(dialog).getByRole('button', { name: 'Run anyway' });
    expect(confirm).toBeDisabled();
    await user.type(within(dialog).getByLabelText(/app_prod/), 'app_prod');
    await user.click(confirm);
    await waitFor(() => expect(getTabData(id)!.run?.status).toBe('ok'));
  });

  it('opens a .sql file in a new tab and saves it back', async () => {
    const user = userEvent.setup();
    await mountEditor('SELECT 1');
    mockSetSqlFileToOpen({ path: '/work/report.sql', text: 'SELECT 42;' });
    await user.click(screen.getByRole('button', { name: /Open .sql file/ }));
    await waitFor(() =>
      expect(useTabsStore.getState().tabs.filter((t) => t.kind === 'editor')).toHaveLength(2),
    );
    const opened = useTabsStore.getState().tabs.find((t) => t.title === 'report.sql');
    expect(opened).toBeDefined();
    if (opened?.kind !== 'editor') throw new Error('expected editor tab');
    const data = getTabData(opened.payload.tabStateId)!;
    expect(data).toMatchObject({ text: 'SELECT 42;', filePath: '/work/report.sql' });

    // Save the first tab to a new file (the mock "dialog" picks a path).
    await user.click(screen.getAllByRole('button', { name: 'Save as…' })[0]!);
    await waitFor(() => expect(screen.getByTestId('file-name')).toBeInTheDocument());
    expect(screen.getByTestId('file-name').textContent).toMatch(/\.sql/);
  });

  it('keeps history and saved queries in the side panel', async () => {
    const user = userEvent.setup();
    const { id } = await mountEditor('SELECT 1');
    await run();
    await waitFor(() => expect(getTabData(id)!.run?.status).toBe('ok'));
    await user.click(screen.getByRole('button', { name: /History and saved queries/ }));
    const entry = await screen.findByTestId('history-entry');
    expect(entry).toHaveTextContent('SELECT 1');

    await user.click(screen.getByRole('tab', { name: 'Saved' }));
    await user.type(screen.getByLabelText('Save the current text'), 'one');
    await user.click(screen.getByRole('button', { name: 'Save query' }));
    const saved = await screen.findByTestId('saved-entry');
    expect(saved).toHaveTextContent('one');
    expect((await ipc.savedQueriesList()).queries[0]).toMatchObject({
      name: 'one',
      text: 'SELECT 1',
    });
  });
});

describe('Explain', () => {
  it('opens a plan tab for the statement at the cursor with the Explain button and the shortcut', async () => {
    const { id } = await mountEditor('SELECT * FROM users');
    await userEvent.click(screen.getByRole('button', { name: /^Explain plan/ }));
    await waitFor(() =>
      expect(useTabsStore.getState().tabs.some((t) => t.kind === 'plan')).toBe(true),
    );
    const plan = useTabsStore.getState().tabs.find((t) => t.kind === 'plan')!;
    expect(plan.payload).toMatchObject({ connectionId: expect.any(String) });
    const entry = Object.values(usePlanStore.getState().entries)[0]!;
    expect(entry.statement).toBe('SELECT * FROM users');
    expect(entry.request?.analyze).toBe(false);
    expect(entry.leaseId).toBe(getTabData(id)!.leaseId);
    await waitFor(() =>
      expect(Object.values(usePlanStore.getState().entries)[0]!.status).toBe('ready'),
    );
  });
});

describe('connection loss and console mode', () => {
  it('shows a lost connection and warns that the open transaction is gone', async () => {
    const { id } = await mountEditor('BEGIN');
    await run();
    await waitFor(() => expect(getTabData(id)!.leaseState?.transaction).toBe('active'));
    // Give the host a moment to subscribe to `lease://state`.
    await new Promise((r) => setTimeout(r, 20));
    act(() => mockBreakLease(getTabData(id)!.leaseId!));
    expect(
      await screen.findByText(/transaction was open|while a transaction was open/i),
    ).toBeInTheDocument();
    expect(screen.getByText(/connection was lost\. Reconnect/i)).toBeInTheDocument();
    expect(getTabData(id)!.transactionLost).toBe(true);
    await userEvent.click(screen.getByRole('button', { name: 'Reconnect' }));
    await waitFor(() => expect(getTabData(id)!.leaseState?.broken).toBe(false));
  });

  it('runs commands in the Redis console and recalls them with ArrowUp', async () => {
    const user = userEvent.setup();
    const base = (await ipc.connectionsList()).profiles[0]!;
    const redis = await ipc.connectionSave(
      {
        ...base,
        id: '00000000-0000-4000-8000-0000000000d1',
        name: 'Redis',
        kind: 'redis',
        port: 6379,
      },
      { action: 'keep' },
    );
    const session = await openSession(redis.id);
    const id = openEditor(session, {});
    const tab = useTabsStore.getState().tabs.find((t) => t.id === editorTabId(id))!;
    if (tab.kind !== 'editor') throw new Error('not an editor tab');
    expect(tab.payload.mode).toBe('redis');
    render(
      <Providers>
        <EditorTab {...tab.payload} />
      </Providers>,
    );
    const input = await screen.findByRole('textbox', { name: 'Console input' });
    await user.type(input, 'GET greeting{Enter}');
    const entry = await screen.findByTestId('console-entry');
    expect(entry).toHaveTextContent('GET greeting');
    // Typed replies come from the Redis console plug-in (quoted like redis-cli).
    expect(within(entry).getByTestId('redis-reply')).toHaveTextContent('"OK"');
    await user.type(input, '{ArrowUp}');
    expect(input).toHaveValue('GET greeting');
  });

  it('suggests Redis commands from script_vocabulary while typing', async () => {
    const user = userEvent.setup();
    const base = (await ipc.connectionsList()).profiles[0]!;
    const redis = await ipc.connectionSave(
      {
        ...base,
        id: '00000000-0000-4000-8000-0000000000d2',
        name: 'Redis',
        kind: 'redis',
        port: 6379,
      },
      { action: 'keep' },
    );
    const session = await openSession(redis.id);
    const id = openEditor(session, {});
    const tab = useTabsStore.getState().tabs.find((t) => t.id === editorTabId(id))!;
    if (tab.kind !== 'editor') throw new Error('not an editor tab');
    render(
      <Providers>
        <EditorTab {...tab.payload} />
      </Providers>,
    );
    const input = await screen.findByRole('textbox', { name: 'Console input' });
    await user.type(input, 'fl');
    const hints = await screen.findByRole('list', { name: 'Command hints' });
    expect(hints).toHaveTextContent('FLUSHDB [ASYNC]');
  });

  it('shows MongoDB results as documents on request', async () => {
    const user = userEvent.setup();
    const base = (await ipc.connectionsList()).profiles[0]!;
    const mongo = await ipc.connectionSave(
      {
        ...base,
        id: '00000000-0000-4000-8000-0000000000d3',
        name: 'Mongo',
        kind: 'mongodb',
        port: 27017,
      },
      { action: 'keep' },
    );
    const session = await openSession(mongo.id);
    const id = openEditor(session, { text: 'db.users.find()' });
    const tab = useTabsStore.getState().tabs.find((t) => t.id === editorTabId(id))!;
    if (tab.kind !== 'editor') throw new Error('not an editor tab');
    render(
      <Providers>
        <EditorTab {...tab.payload} />
      </Providers>,
    );
    await screen.findByTestId('sql-editor', undefined, { timeout: 5000 });
    await user.click(screen.getByRole('button', { name: /^Run (statement|selection)$/ }));
    await user.click(await screen.findByRole('button', { name: 'Documents' }, { timeout: 5000 }));
    expect(await screen.findByTestId('mongo-documents')).toHaveTextContent('"name": "doc 1"');
  });

  it('shows the matched/modified counts of a MongoDB update next to the affected rows', async () => {
    const user = userEvent.setup();
    const base = (await ipc.connectionsList()).profiles[0]!;
    const mongo = await ipc.connectionSave(
      {
        ...base,
        id: '00000000-0000-4000-8000-0000000000d4',
        name: 'Mongo',
        kind: 'mongodb',
        port: 27017,
      },
      { action: 'keep' },
    );
    const session = await openSession(mongo.id);
    const id = openEditor(session, {
      text: "db.trips.updateOne({ code: 1, status: 'searching' }, { $set: { status: 'accepted' } })",
    });
    const tab = useTabsStore.getState().tabs.find((t) => t.id === editorTabId(id))!;
    if (tab.kind !== 'editor') throw new Error('not an editor tab');
    render(
      <Providers>
        <EditorTab {...tab.payload} />
        <ConfirmDestructiveHost />
      </Providers>,
    );
    await screen.findByTestId('sql-editor', undefined, { timeout: 5000 });
    await user.click(screen.getByRole('button', { name: /^Run (statement|selection)$/ }));
    const summary = await screen.findByTestId('result-summary', undefined, { timeout: 5000 });
    expect(summary).toHaveTextContent('0 row(s) affected');
    expect(within(summary).getByTestId('result-summary-messages')).toHaveTextContent(
      'matched: 0, modified: 0',
    );
  });
});

describe('transaction cell of the status bar', () => {
  it('follows the active editor tab', async () => {
    const session = await openSession();
    const id = openEditor(session, { text: '' });
    patchTabData(id, { autoCommit: false });
    render(
      <Providers>
        <StatusBar />
      </Providers>,
    );
    expect(await screen.findByTestId('status-transaction')).toHaveTextContent(
      'Manual: no transaction',
    );
    patchTabData(id, {
      leaseState: {
        autoCommit: false,
        transaction: 'active',
        running: null,
        pendingResult: null,
        database: null,
        schema: null,
        broken: false,
      },
    });
    await waitFor(() =>
      expect(screen.getByTestId('status-transaction')).toHaveTextContent('Transaction open'),
    );
    act(() => useTabsStore.getState().activate('welcome'));
    await waitFor(() => expect(screen.queryByTestId('status-transaction')).toBeNull());
  });
});

describe('lost session (F2d)', () => {
  afterEach(() => vi.restoreAllMocks());

  it('Run reports a lost session with a reconnect button, not "Nothing to run"', async () => {
    useToastStore.setState({ toasts: [] });
    const { session } = await mountEditor('SELECT 1');
    render(<Toaster />);
    vi.spyOn(ipc, 'scriptAnalyze').mockRejectedValue({
      kind: 'notFound',
      message: `session ${session.sessionId}`,
    });
    const reconnect = vi.spyOn(ipc, 'sessionReconnect');
    await run();
    expect(await screen.findByText('The connection session was lost.')).toBeInTheDocument();
    expect(screen.queryByText('Nothing to run.')).toBeNull();
    await userEvent.click(screen.getByRole('button', { name: 'Reconnect' }));
    await waitFor(() => expect(reconnect).toHaveBeenCalledWith(session.sessionId));
    expect(await screen.findByText('Reconnected.')).toBeInTheDocument();
  });

  it('Explain reports a lost session too', async () => {
    useToastStore.setState({ toasts: [] });
    await mountEditor('SELECT 1');
    render(<Toaster />);
    vi.spyOn(ipc, 'scriptAnalyze').mockRejectedValue({ kind: 'notFound', message: 'gone' });
    await userEvent.click(screen.getByRole('button', { name: /^Explain/ }));
    expect(await screen.findByText('The connection session was lost.')).toBeInTheDocument();
    expect(screen.queryByText('Nothing to run.')).toBeNull();
  });

  it('shows the real message of any other analysis error', async () => {
    useToastStore.setState({ toasts: [] });
    await mountEditor('SELECT 1');
    render(<Toaster />);
    vi.spyOn(ipc, 'scriptAnalyze').mockRejectedValue({ kind: 'other', message: 'parser exploded' });
    await run();
    expect(await screen.findByText('parser exploded')).toBeInTheDocument();
    expect(screen.queryByText('Nothing to run.')).toBeNull();
    expect(screen.queryByRole('button', { name: 'Reconnect' })).toBeNull();
  });

  it('still says "Nothing to run" when the analysis works but nothing is selected', async () => {
    useToastStore.setState({ toasts: [] });
    await mountEditor('   ');
    render(<Toaster />);
    await run();
    expect(await screen.findByText('Nothing to run.')).toBeInTheDocument();
  });
});
