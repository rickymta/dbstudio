import { EditorView } from '@codemirror/view';
import { act, cleanup, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { resetDataStores } from '@/features/data/store';
import { polyfillLayoutApis, renderWithProviders } from '@/features/data/testUtils';
import { ipc } from '@/lib/ipc';
import { DocumentView } from './DocumentView';
import { resetDocStores } from './store';
import { mongoPath, openMongoSession, resetMongoMocks } from './testUtils';

beforeAll(() => polyfillLayoutApis());
beforeEach(() => resetMongoMocks());
afterEach(() => {
  cleanup();
  resetDataStores();
  resetDocStores();
  vi.restoreAllMocks();
});

async function mount(collection = 'users', opts: { readOnly?: boolean } = {}) {
  const { sessionId, connectionId } = await openMongoSession(opts);
  renderWithProviders(
    <DocumentView sessionId={sessionId} connectionId={connectionId} path={mongoPath(collection)} />,
  );
  await waitFor(() => expect(screen.getByText('30 rows loaded')).toBeInTheDocument(), {
    timeout: 5000,
  });
  return { sessionId, connectionId };
}

/** Select the first document row and wait for its editability to be known. */
async function selectFirst(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getAllByRole('gridcell')[1]!);
  await waitFor(() => expect(screen.getByRole('button', { name: /Edit/ })).toBeEnabled());
}

describe('DocumentView (mock IPC)', () => {
  it('shows a flat table with the union of keys, _id first', async () => {
    await mount();
    const headers = screen.getAllByRole('columnheader').map((h) => h.textContent);
    expect(headers.slice(0, 4)).toEqual(['#', '_id', 'name', 'email']);
    expect(headers).toContain('address');
    expect(headers).toContain('vip');
    // Nested values are summarized, ObjectIds keep their type badge.
    expect(screen.getAllByText('{2 fields}').length).toBeGreaterThan(0);
    expect(screen.getAllByText('[2 items]').length).toBeGreaterThan(0);
    expect(screen.getAllByText('objectId').length).toBeGreaterThan(0);
    expect(screen.getAllByText('null').length).toBeGreaterThan(0);
  });

  it('passes the filter and sort as dotted paths', async () => {
    const spy = vi.spyOn(ipc, 'tableFetch');
    const user = userEvent.setup();
    await mount();
    await user.type(screen.getByLabelText('Filter (EJSON)'), '{{"age": {{"$gt": 50}}{Enter}');
    await waitFor(() => expect(spy.mock.calls.length).toBeGreaterThan(1));
    expect(spy.mock.calls.at(-1)![1].rawWhere).toBe('{"age": {"$gt": 50}}');

    await user.click(await screen.findByRole('button', { name: 'age' }));
    await waitFor(() =>
      expect(spy.mock.calls.at(-1)![1].sort).toEqual([{ column: 'age', descending: false }]),
    );
  });

  it('shows the selected document as a tree and as EJSON', async () => {
    const user = userEvent.setup();
    await mount();
    await user.click(screen.getAllByRole('gridcell')[1]!);
    await user.click(screen.getByRole('button', { name: 'Tree' }));
    expect(screen.getByTestId('document-tree')).toBeInTheDocument();
    expect(screen.getByText('1 of 30')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Next document' }));
    expect(screen.getByText('2 of 30')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'JSON' }));
    const host = await screen.findByTestId('json-editor');
    const view = EditorView.findFromDOM(host)!;
    await waitFor(() => expect(view.state.doc.toString()).toContain('"$oid"'));
  });

  it('adds a document through the editor and reloads', async () => {
    const user = userEvent.setup();
    await mount();
    await waitFor(() => expect(screen.getByRole('button', { name: /Add document/ })).toBeEnabled());
    await user.click(screen.getByRole('button', { name: /Add document/ }));
    const dialog = await screen.findByRole('dialog');
    await within(dialog).findByTestId('json-editor');
    expect(within(dialog).getByText('Valid JSON')).toBeInTheDocument();
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(screen.getByText('31 rows loaded')).toBeInTheDocument(), {
      timeout: 5000,
    });
  });

  it('locks editing on a read-only connection', async () => {
    await mount('users', { readOnly: true });
    expect(
      await screen.findByText('Editing is off: the connection is read-only.'),
    ).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Add document/ })).toBeDisabled();
    expect(screen.getByRole('button', { name: /Delete/ })).toBeDisabled();
  });

  it('deletes the selected document after confirming', async () => {
    const user = userEvent.setup();
    const { sessionId } = await mount();
    const apply = vi.spyOn(ipc, 'changesApply');
    await selectFirst(user);
    await user.click(screen.getByRole('button', { name: /Delete/ }));
    const dialog = await screen.findByRole('dialog');
    await waitFor(() => expect(within(dialog).getByText(/deleteOne/)).toBeInTheDocument());
    await user.click(within(dialog).getByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(apply).toHaveBeenCalled());
    expect(apply.mock.calls[0]![0]).toBe(sessionId);
    expect(apply.mock.calls[0]![1]).toMatchObject({
      model: 'documents',
      changes: [{ op: 'delete' }],
    });
    await waitFor(() => expect(screen.getByText('29 rows loaded')).toBeInTheDocument(), {
      timeout: 5000,
    });
  });
});

describe('DocumentEditor flow', () => {
  it('replaces a document with the edited EJSON', async () => {
    const user = userEvent.setup();
    await mount();
    const apply = vi.spyOn(ipc, 'changesApply');
    await selectFirst(user);
    await user.click(screen.getByRole('button', { name: /Edit/ }));
    const dialog = await screen.findByRole('dialog');
    const host = await within(dialog).findByTestId('json-editor');
    const view = EditorView.findFromDOM(host)!;
    await waitFor(() => expect(view.state.doc.toString()).toContain('"_id"'));
    expect(within(dialog).getByText('Valid JSON')).toBeInTheDocument();

    await user.click(within(dialog).getByRole('button', { name: 'Preview command' }));
    await waitFor(() => expect(within(dialog).getByText(/replaceOne/)).toBeInTheDocument());

    // Invalid JSON shows the position and blocks saving.
    const set = (text: string) =>
      act(() => view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text } }));
    set('{"a": }');
    expect(await within(dialog).findByText(/Line 1, column 7/)).toBeInTheDocument();
    expect(within(dialog).getByRole('button', { name: 'Save' })).toBeDisabled();

    set('{"a": 1}');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(apply).toHaveBeenCalledTimes(1));
    expect(apply.mock.calls[0]![1]).toMatchObject({
      changes: [{ op: 'replace', document: { kind: 'ejson', text: '{"a": 1}' } }],
    });
  });
});
