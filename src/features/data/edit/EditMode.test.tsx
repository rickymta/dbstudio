import { act, cleanup, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { ipc } from '@/lib/ipc';
import { TableTab } from '../TableTab';
import { peekGridModel, resetDataStores } from '../store';
import { tableTabId } from '@/stores/tabs';
import {
  PG_CONNECTION,
  openMockSession,
  pgPath,
  polyfillLayoutApis,
  renderWithProviders,
} from '../testUtils';

let sessionId: string;
beforeAll(async () => {
  polyfillLayoutApis();
  sessionId = await openMockSession();
});
afterEach(() => {
  cleanup();
  resetDataStores();
  vi.restoreAllMocks();
});

// `products`: id (identity, read-only), sku, name, price, created_on, attrs
const payload = (table = 'products', kind: 'table' | 'view' = 'table') => ({
  sessionId,
  connectionId: PG_CONNECTION,
  path: pgPath(table, kind),
});

const cell = (r: number, c: number) =>
  document.querySelector<HTMLElement>(`[role="gridcell"][data-r="${r}"][data-c="${c}"]`)!;
const rowEl = (r: number) => cell(r, 1).closest<HTMLElement>('[role="row"]')!;

async function openEditMode(user: ReturnType<typeof userEvent.setup>) {
  renderWithProviders(<TableTab {...payload()} />);
  await waitFor(() => expect(screen.getByText('250 rows loaded')).toBeInTheDocument(), {
    timeout: 5000,
  });
  await user.click(await screen.findByRole('button', { name: /Edit data/ }));
}

describe('edit mode', () => {
  it('tints edited cells and keeps NULL and the empty text apart', async () => {
    const user = userEvent.setup();
    await openEditMode(user);

    // sku -> '' (empty text), name -> NULL, price -> a big decimal typed as text.
    await user.dblClick(cell(0, 1));
    const sku = screen.getByRole('textbox', { name: 'Value of sku' });
    await user.clear(sku);
    await user.type(sku, '{Enter}');
    expect(cell(0, 1)).toHaveAttribute('data-pending', 'updated');
    expect(cell(0, 1).className).toContain('bg-warning-soft');
    expect(cell(0, 1).textContent).toBe('');

    await user.dblClick(cell(0, 2));
    await screen.findByRole('textbox', { name: 'Value of name' });
    await user.click(screen.getByRole('button', { name: 'NULL' }));
    expect(cell(0, 2)).toHaveAttribute('data-pending', 'updated');
    expect(cell(0, 2).textContent).toBe('NULL');

    await user.dblClick(cell(0, 3));
    const price = screen.getByRole('textbox', { name: 'Value of price' });
    await user.clear(price);
    await user.type(price, '12345678901234567890.55{Enter}');
    expect(cell(0, 3).textContent).toBe('12345678901234567890.55');

    expect(rowEl(0)).toHaveAttribute('data-pending', 'updated');
    expect(cell(1, 1)).not.toHaveAttribute('data-pending');
    expect(screen.getByTestId('pending-count')).toHaveTextContent('1 pending change');

    // The preview shows one UPDATE where '' and NULL are different literals.
    await user.click(screen.getByRole('button', { name: /Preview/ }));
    const list = await screen.findByTestId('preview-statements');
    const sql = within(list).getAllByRole('listitem')[0]!.textContent!;
    expect(sql).toContain('"sku" = \'\'');
    expect(sql).toContain('"name" = NULL');
    expect(sql).toContain('"price" = \'12345678901234567890.55\'');
    expect(sql).toContain('WHERE "id" = 1');
  });

  it('rejects an invalid number and keeps the cell unchanged on Escape', async () => {
    const user = userEvent.setup();
    await openEditMode(user);
    const before = cell(0, 3).textContent;
    await user.dblClick(cell(0, 3));
    const price = screen.getByRole('textbox', { name: 'Value of price' });
    await user.clear(price);
    await user.type(price, 'abc{Enter}');
    expect(price).toHaveAttribute('aria-invalid', 'true');
    await user.keyboard('{Escape}');
    expect(screen.queryByRole('textbox', { name: 'Value of price' })).toBeNull();
    expect(cell(0, 3).textContent).toBe(before);
    expect(cell(0, 3)).not.toHaveAttribute('data-pending');
  });

  it('does not edit generated columns', async () => {
    const user = userEvent.setup();
    await openEditMode(user);
    await user.dblClick(cell(0, 0));
    expect(screen.queryByRole('textbox')).toBeNull();
  });

  it('adds, duplicates and deletes rows through the pending list', async () => {
    const user = userEvent.setup();
    await openEditMode(user);

    // New rows come first, so loading more pages never moves them.
    await user.click(screen.getByRole('button', { name: /Add row/ }));
    expect(screen.getByRole('grid')).toHaveAttribute('aria-rowcount', '252');
    expect(rowEl(0)).toHaveAttribute('data-pending', 'inserted');
    expect(screen.getByTestId('pending-count')).toHaveTextContent('1 pending change');

    // Duplicate the first loaded row (now row 1): its key becomes DEFAULT, the generated id
    // column is not copied.
    await user.click(cell(1, 1));
    await user.click(screen.getByRole('button', { name: /Duplicate row/ }));
    expect(screen.getByTestId('pending-count')).toHaveTextContent('2 pending changes');

    await user.click(cell(3, 1)); // a loaded row
    await user.click(screen.getByRole('button', { name: /Delete row/ }));
    expect(rowEl(3)).toHaveAttribute('data-pending', 'deleted');
    expect(screen.getByTestId('pending-count')).toHaveTextContent('3 pending changes');
    // Deleting twice is an undo.
    await user.click(screen.getByRole('button', { name: /Undo delete/ }));
    expect(rowEl(3)).not.toHaveAttribute('data-pending');

    const preview = vi.spyOn(ipc, 'changesPreview');
    await user.click(screen.getByRole('button', { name: /Preview/ }));
    await screen.findByTestId('preview-statements');
    const sent = preview.mock.calls.at(-1)![1];
    expect(sent.model === 'rows' && sent.changes.map((c) => c.op)).toEqual(['insert', 'insert']);
  });

  it('shows new and deleted rows with their own state, then discards everything', async () => {
    const user = userEvent.setup();
    // `categories` is small enough for every row to be inside the rendered window.
    renderWithProviders(<TableTab {...payload('categories')} />);
    await waitFor(() => expect(screen.getByText('24 rows loaded')).toBeInTheDocument(), {
      timeout: 5000,
    });
    await user.click(await screen.findByRole('button', { name: /Edit data/ }));
    await user.click(screen.getByRole('button', { name: /Add row/ }));
    await user.click(cell(3, 1));
    await user.click(screen.getByRole('button', { name: /Delete row/ }));
    await waitFor(() => expect(rowEl(0)).toHaveAttribute('data-pending', 'inserted'));
    expect(rowEl(3)).toHaveAttribute('data-pending', 'deleted');
    expect(rowEl(0).className).toContain('bg-success-soft');
    expect(rowEl(3).className).toContain('bg-danger-soft');
    // Cells nobody typed in show the column default.
    expect(cell(0, 1).textContent).toBe('DEFAULT');

    vi.spyOn(window, 'confirm').mockReturnValue(true);
    await user.click(screen.getByRole('button', { name: /Discard/ }));
    expect(screen.getByTestId('pending-count')).toHaveTextContent('No pending changes');
    expect(rowEl(3)).not.toHaveAttribute('data-pending');
    expect(screen.getByRole('grid')).toHaveAttribute('aria-rowcount', '25');
  });

  it('keeps a typed value in the new row, and a reload closes an open editor', async () => {
    const user = userEvent.setup();
    renderWithProviders(<TableTab {...payload('categories')} />);
    await waitFor(() => expect(screen.getByText('24 rows loaded')).toBeInTheDocument(), {
      timeout: 5000,
    });
    await user.click(await screen.findByRole('button', { name: /Edit data/ }));
    await user.click(screen.getByRole('button', { name: /Add row/ }));
    await user.dblClick(cell(0, 1));
    const box = screen.getByRole('textbox', { name: 'Value of name' });
    await user.type(box, 'zzz{Enter}');
    expect(cell(0, 1).textContent).toBe('zzz');
    expect(rowEl(0)).toHaveAttribute('data-pending', 'inserted');
    expect(rowEl(1)).not.toHaveAttribute('data-pending');

    // An editor open on a row index is closed by a reload instead of writing into another row.
    await user.dblClick(cell(0, 1));
    expect(screen.getByRole('textbox', { name: 'Value of name' })).toBeInTheDocument();
    const model = peekGridModel(tableTabId(sessionId, pgPath('categories')))!;
    act(() => model.reload());
    await waitFor(() =>
      expect(screen.queryByRole('textbox', { name: 'Value of name' })).toBeNull(),
    );
    await waitFor(() => expect(screen.getByText('24 rows loaded')).toBeInTheDocument());
    expect(screen.getByTestId('pending-count')).toHaveTextContent('1 pending change');
  });

  it('leaving a NULL cell untouched keeps it NULL', async () => {
    const user = userEvent.setup();
    renderWithProviders(<TableTab {...payload('categories')} />);
    await waitFor(() => expect(screen.getByText('24 rows loaded')).toBeInTheDocument(), {
      timeout: 5000,
    });
    await user.click(await screen.findByRole('button', { name: /Edit data/ }));
    let r = 0;
    while (r < 24 && cell(r, 2)?.textContent !== 'NULL') r++;
    expect(r).toBeLessThan(24);
    await user.dblClick(cell(r, 2));
    await user.keyboard('{Enter}');
    expect(screen.queryByRole('textbox')).toBeNull();
    expect(cell(r, 2)).not.toHaveAttribute('data-pending');
    expect(screen.getByTestId('pending-count')).toHaveTextContent('No pending changes');
  });

  it('locks the grid while changes are being applied', async () => {
    const user = userEvent.setup();
    await openEditMode(user);
    let finish!: () => void;
    vi.spyOn(ipc, 'changesApply').mockImplementation(
      () =>
        new Promise((resolve) => {
          finish = () => resolve({ applied: 1, affected: [1], atomic: true });
        }),
    );
    await user.dblClick(cell(0, 2));
    const box = screen.getByRole('textbox', { name: 'Value of name' });
    await user.type(box, 'x{Enter}');
    await user.click(
      within(screen.getByTestId('edit-bar')).getByRole('button', { name: /^Apply$/ }),
    );
    expect(screen.getByRole('button', { name: /Add row/ })).toBeDisabled();
    await user.dblClick(cell(1, 2));
    expect(screen.queryByRole('textbox')).toBeNull();
    act(() => finish());
    await waitFor(() =>
      expect(screen.getByTestId('pending-count')).toHaveTextContent('No pending changes'),
    );
    expect(screen.getByRole('button', { name: /Add row/ })).toBeEnabled();
  });

  it('warns in the preview when key values are edited', async () => {
    vi.spyOn(ipc, 'tableEditability').mockResolvedValue({
      editable: true,
      keyColumns: ['id'],
      readOnlyColumns: [],
      reason: null,
      atomic: true,
    });
    const user = userEvent.setup();
    await openEditMode(user);
    await user.dblClick(cell(0, 0));
    const box = screen.getByRole('textbox', { name: 'Value of id' });
    await user.clear(box);
    await user.type(box, '9999{Enter}');
    await user.click(screen.getByRole('button', { name: /Preview/ }));
    expect(await screen.findByText(/Changing key values/)).toBeInTheDocument();
  });

  it('applies and reloads; a failing change stays pending and points at its row', async () => {
    const user = userEvent.setup();
    await openEditMode(user);
    const apply = vi.spyOn(ipc, 'changesApply');

    await user.dblClick(cell(3, 2));
    const name = screen.getByRole('textbox', { name: 'Value of name' });
    await user.clear(name);
    await user.type(name, '__fail__{Enter}');
    await user.click(
      within(screen.getByTestId('edit-bar')).getByRole('button', { name: /^Apply$/ }),
    );
    await waitFor(() => expect(apply).toHaveBeenCalledTimes(1));
    expect(await screen.findByText(/Change 1 failed and nothing was saved/)).toBeInTheDocument();
    expect(rowEl(3)).toHaveAttribute('data-failed', 'true');
    expect(screen.getByTestId('pending-count')).toHaveTextContent('1 pending change');

    // Fix the value; the next apply succeeds, clears the pending list and reloads the page.
    const fetchSpy = vi.spyOn(ipc, 'tableFetch');
    await user.dblClick(cell(3, 2));
    const again = screen.getByRole('textbox', { name: 'Value of name' });
    await user.clear(again);
    await user.type(again, 'fine{Enter}');
    await user.click(
      within(screen.getByTestId('edit-bar')).getByRole('button', { name: /^Apply$/ }),
    );
    await waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    await waitFor(() =>
      expect(screen.getByTestId('pending-count')).toHaveTextContent('No pending changes'),
    );
    expect(screen.queryByText(/Change 1 failed/)).toBeNull();
  });
});

describe('read-only tables', () => {
  it.each(['noUniqueKey', 'readOnlyConnection', 'notATable'] as const)(
    'explains why a table is not editable (%s)',
    async (reason) => {
      vi.spyOn(ipc, 'tableEditability').mockResolvedValue({
        editable: false,
        keyColumns: [],
        readOnlyColumns: [],
        reason,
        atomic: true,
      });
      renderWithProviders(<TableTab {...payload()} />);
      await waitFor(() => expect(screen.getByText('250 rows loaded')).toBeInTheDocument(), {
        timeout: 5000,
      });
      const note = await screen.findByTestId('edit-readonly-reason');
      expect(note).toHaveAttribute('data-reason', reason);
      expect(note.textContent).toMatch(/^Read-only: /);
      expect(screen.queryByRole('button', { name: /Edit data/ })).toBeNull();
      expect(screen.queryByRole('button', { name: /Add row/ })).toBeNull();
    },
  );

  it('lists views as not editable in the mock', async () => {
    renderWithProviders(<TableTab {...payload('active_users', 'view')} />);
    const note = await screen.findByTestId('edit-readonly-reason');
    expect(note).toHaveAttribute('data-reason', 'notATable');
  });
});
