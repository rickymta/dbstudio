import { act, cleanup, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { ipc } from '@/lib/ipc';
import { pathKey } from '@/lib/objectPath';
import { layoutStorageKey } from './columnLayout';
import { tableTabId, useTabsStore } from '@/stores/tabs';
import { DdlTab } from './DdlTab';
import { TableTab } from './TableTab';
import { peekGridModel, resetDataStores } from './store';
import {
  PG_CONNECTION,
  openMockSession,
  pgPath,
  polyfillLayoutApis,
  renderWithProviders,
} from './testUtils';

let sessionId: string;
beforeAll(async () => {
  polyfillLayoutApis();
  sessionId = await openMockSession();
});
afterEach(() => {
  cleanup(); // unmount first: resetting stores under a mounted tab would recreate its state
  resetDataStores();
  vi.restoreAllMocks();
});

const payload = () => ({
  sessionId,
  connectionId: PG_CONNECTION,
  path: pgPath('events'),
});

describe('TableTab (mock IPC)', () => {
  it('renders the first page from the mock and keeps its state across unmount/mount', async () => {
    const fetchSpy = vi.spyOn(ipc, 'tableFetch');
    const first = renderWithProviders(<TableTab {...payload()} />);

    // Header: breadcrumb + sub-tabs.
    expect(screen.getByText('events')).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: 'Data' })).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: 'Structure' })).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: 'DDL' })).toBeInTheDocument();

    await waitFor(() => expect(screen.getByText('1,000 rows loaded')).toBeInTheDocument(), {
      timeout: 5000,
    });
    expect(fetchSpy).toHaveBeenCalledTimes(1);
    expect(fetchSpy.mock.calls[0]![1]).toMatchObject({ offset: 0, limit: 1000 });

    // Cells are virtualized: some, but far fewer than 1000 rows x 12 columns, are in the DOM.
    const cells = screen.getAllByRole('gridcell');
    expect(cells.length).toBeGreaterThan(10);
    expect(cells.length).toBeLessThan(1000 * 12);
    expect(screen.getByRole('columnheader', { name: /^id/ })).toBeInTheDocument();
    // Beyond 2^53 stays verbatim.
    expect(screen.getAllByText('9007199254740993').length).toBeGreaterThan(0);
    expect(screen.getAllByText('NULL').length).toBeGreaterThan(0);

    // Select a cell and open the viewer.
    const user = userEvent.setup();
    await user.dblClick(cells[1]!);
    expect(screen.getByRole('complementary', { name: 'Cell' })).toBeInTheDocument();

    first.unmount();
    expect(peekGridModel(tableTabId(sessionId, pgPath('events')))).toBeDefined();

    renderWithProviders(<TableTab {...payload()} />);
    // Rows, selection and the open viewer are all still there; nothing was refetched.
    expect(screen.getByText('1,000 rows loaded')).toBeInTheDocument();
    expect(screen.getByRole('complementary', { name: 'Cell' })).toBeInTheDocument();
    expect(screen.getAllByRole('gridcell').length).toBeGreaterThan(10);
    expect(fetchSpy).toHaveBeenCalledTimes(1);
  });

  it('sorts server-side from a header click', async () => {
    const fetchSpy = vi.spyOn(ipc, 'tableFetch');
    const user = userEvent.setup();
    renderWithProviders(<TableTab {...payload()} />);
    await waitFor(() => expect(screen.getByText('1,000 rows loaded')).toBeInTheDocument(), {
      timeout: 5000,
    });
    await user.click(screen.getByRole('columnheader', { name: /^id/ }));
    await user.click(screen.getByRole('columnheader', { name: /^id/ }));
    await waitFor(() => expect(screen.getByText('1,000 rows loaded')).toBeInTheDocument(), {
      timeout: 5000,
    });
    const last = fetchSpy.mock.calls.at(-1)![1];
    expect(last.sort).toEqual([{ column: 'id', descending: true }]);
    expect(last.offset).toBe(0);
    expect(screen.getByRole('columnheader', { name: /^id/ })).toHaveAttribute(
      'aria-sort',
      'descending',
    );
  });

  it('clears the selection on reload and exposes row/active-cell semantics', async () => {
    const user = userEvent.setup();
    renderWithProviders(<TableTab {...payload()} />);
    await waitFor(() => expect(screen.getByText('1,000 rows loaded')).toBeInTheDocument(), {
      timeout: 5000,
    });
    const grid = screen.getByRole('grid');
    expect(grid).toHaveAttribute('aria-rowcount', '1001'); // data rows + header row
    expect(screen.getAllByRole('row')[0]).toHaveAttribute('aria-rowindex', '1');
    expect(screen.getAllByRole('row')[1]).toHaveAttribute('aria-rowindex', '2');
    expect(grid).not.toHaveAttribute('aria-activedescendant');

    const cell = screen.getAllByRole('gridcell')[2]!;
    await user.click(cell);
    expect(grid.getAttribute('aria-activedescendant')).toBe(cell.id);

    await user.click(screen.getByRole('columnheader', { name: /^id/ }));
    await waitFor(() => expect(screen.getByText('1,000 rows loaded')).toBeInTheDocument(), {
      timeout: 5000,
    });
    expect(screen.getByRole('grid')).not.toHaveAttribute('aria-activedescendant');
    expect(screen.queryByRole('gridcell', { selected: true })).toBeNull();
  });

  it('counts on demand only', async () => {
    const countSpy = vi.spyOn(ipc, 'tableCount');
    const user = userEvent.setup();
    renderWithProviders(<TableTab {...payload()} />);
    await waitFor(() => expect(screen.getByText('1,000 rows loaded')).toBeInTheDocument(), {
      timeout: 5000,
    });
    expect(countSpy).not.toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: 'Count' }));
    await waitFor(() => expect(screen.getByText(/of 1,000,000/)).toBeInTheDocument());
    expect(countSpy).toHaveBeenCalledTimes(1);
  });

  it('loads Structure and DDL only when opened', async () => {
    const detailsSpy = vi.spyOn(ipc, 'tableDetails');
    const ddlSpy = vi.spyOn(ipc, 'objectDdl');
    const user = userEvent.setup();
    renderWithProviders(<TableTab {...payload()} />);
    expect(detailsSpy).not.toHaveBeenCalled();
    expect(ddlSpy).not.toHaveBeenCalled();
    await user.click(screen.getByRole('tab', { name: 'Structure' }));
    expect(await screen.findByRole('table', { name: 'Columns' })).toBeInTheDocument();
    expect(ddlSpy).not.toHaveBeenCalled();
    await user.click(screen.getByRole('tab', { name: 'DDL' }));
    expect(
      await screen.findByTestId('code-view', undefined, { timeout: 5000 }),
    ).toBeInTheDocument();
    expect(ddlSpy).toHaveBeenCalledTimes(1);
  });

  it('drops its state when the tab is closed', async () => {
    const id = tableTabId(sessionId, pgPath('events'));
    act(() => {
      useTabsStore.getState().openTab({
        id,
        kind: 'table',
        title: 'events',
        closable: true,
        payload: payload(),
      });
    });
    const view = renderWithProviders(<TableTab {...payload()} />);
    await waitFor(() => expect(peekGridModel(id)).toBeDefined());
    view.unmount(); // the app unmounts a closed tab's component
    act(() => useTabsStore.getState().closeTab(id));
    expect(peekGridModel(id)).toBeUndefined();
  });
});

describe('DdlTab', () => {
  it('shows the DDL of the object', async () => {
    renderWithProviders(<DdlTab {...payload()} />);
    expect(screen.getByTestId('ddl-tab')).toBeInTheDocument();
    const editor = await screen.findByTestId('code-view', undefined, { timeout: 5000 });
    await waitFor(() => expect(editor.textContent).toContain('CREATE TABLE'));
  });
});

describe('TableTab column layout, settings and timestamps', () => {
  const settings = (over: Record<string, unknown>) => ({
    logLevel: 'info',
    gridMaxRows: 1_000_000,
    gridMaxCells: 20_000_000,
    timestampDisplay: 'utc',
    ...over,
  });

  async function loaded() {
    await waitFor(() => expect(screen.getByText('1,000 rows loaded')).toBeInTheDocument(), {
      timeout: 5000,
    });
  }

  it('hides and pins columns, remembers them per table, and restores after a fresh mount', async () => {
    localStorage.clear();
    const user = userEvent.setup();
    const first = renderWithProviders(<TableTab {...payload()} />);
    await loaded();
    expect(screen.getByRole('columnheader', { name: /^kind/ })).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Columns' }));
    await user.click(screen.getByRole('checkbox', { name: 'kind' }));
    expect(screen.queryByRole('columnheader', { name: /^kind/ })).toBeNull();
    await user.click(screen.getByRole('button', { name: 'Pin left: amount' }));
    await user.keyboard('{Escape}');

    // Pinned column is rendered in the sticky block, before the scrolling ones.
    const headers = screen.getAllByRole('columnheader').map((h) => h.textContent);
    expect(headers[0]).toMatch(/^amount/);
    expect(headers.some((h) => h?.startsWith('kind'))).toBe(false);

    const key = Object.keys(localStorage).find((k) => k.startsWith('dbstudio.gridLayout.'))!;
    expect(JSON.parse(localStorage.getItem(key)!)).toMatchObject({
      hidden: ['kind'],
      pinned: ['amount'],
    });

    // Selection / viewer use the real column, not the display position.
    const cell = screen.getAllByRole('gridcell').find((c) => c.dataset.c === '3')!; // amount
    await user.dblClick(cell);
    expect(screen.getByRole('complementary', { name: 'Cell' })).toHaveTextContent('amount');

    // A closed tab loses its state; a new tab for the same table reads the layout back.
    first.unmount();
    resetDataStores();
    renderWithProviders(<TableTab {...payload()} />);
    await loaded();
    expect(screen.queryByRole('columnheader', { name: /^kind/ })).toBeNull();
    expect(screen.getAllByRole('columnheader')[0]!.textContent).toMatch(/^amount/);

    // Reset layout brings everything back and clears the stored layout.
    await user.click(screen.getByRole('button', { name: 'Columns' }));
    await user.click(screen.getByRole('button', { name: 'Reset layout' }));
    expect(screen.getByRole('columnheader', { name: /^kind/ })).toBeInTheDocument();
    expect(JSON.parse(localStorage.getItem(key)!)).toMatchObject({ hidden: [], pinned: [] });
  });

  it('applies the row limit from the settings and names it in the banner', async () => {
    vi.spyOn(ipc, 'settingsGet').mockResolvedValue(settings({ gridMaxRows: 1000 }) as never);
    renderWithProviders(<TableTab {...payload()} />);
    await loaded();
    await waitFor(() =>
      expect(screen.getByText(/Loading stopped at 1,000 rows/)).toBeInTheDocument(),
    );
    expect(screen.getByText(/limit in Settings/)).toBeInTheDocument();
  });

  it('shows timestamps in local time with an explicit offset when configured', async () => {
    vi.stubEnv('TZ', 'Asia/Kolkata');
    vi.spyOn(ipc, 'settingsGet').mockResolvedValue(
      settings({ timestampDisplay: 'local' }) as never,
    );
    // Keep only two columns so the timestamp column is inside the (jsdom) viewport.
    const names = [
      'big_ref',
      'user_id',
      'amount',
      'kind',
      'message',
      'is_error',
      'event_date',
      'trace_id',
      'payload',
      'blob',
    ];
    localStorage.setItem(
      layoutStorageKey(PG_CONNECTION, pathKey(pgPath('events'))),
      JSON.stringify({ order: [], hidden: names, pinned: [], widths: {} }),
    );
    try {
      renderWithProviders(<TableTab {...payload()} />);
      await loaded();
      await waitFor(() =>
        expect(
          screen.getAllByText(/^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d(\.\d+)?\+05:30$/).length,
        ).toBeGreaterThan(0),
      );
    } finally {
      vi.unstubAllEnvs();
    }
  });
});
