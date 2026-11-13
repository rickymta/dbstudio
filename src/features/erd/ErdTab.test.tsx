import { cleanup, fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import {
  PG_CONNECTION,
  openMockSession,
  polyfillLayoutApis,
  renderWithProviders,
} from '@/features/data/testUtils';
import { ipc } from '@/lib/ipc';
import { resetMockErd } from '@/lib/mock/erd';
import type { ErdLayout, ObjectPath } from '@/lib/types';
import { ErdTab } from './ErdTab';
import { parseLayoutTables, tableKey } from './model';

// React Flow measures with DOMMatrixReadOnly, which jsdom does not have.
class FakeMatrix {
  m22 = 1;
}
beforeAll(() => {
  polyfillLayoutApis();
  (window as unknown as { DOMMatrixReadOnly: unknown }).DOMMatrixReadOnly = FakeMatrix;
  // The mock session helper must exist before the first render.
});

let sessionId = '';
const scope: ObjectPath = { segments: [{ kind: 'database', name: 'app_dev' }] };

async function mount(layoutId: string | null = null) {
  sessionId ||= await openMockSession();
  return renderWithProviders(
    <ErdTab sessionId={sessionId} connectionId={PG_CONNECTION} scope={scope} layoutId={layoutId} />,
  );
}

afterEach(() => {
  cleanup();
  resetMockErd();
  vi.restoreAllMocks();
});

describe('ErdTab (mock IPC)', () => {
  it('draws the tables and the relations of the schema', async () => {
    await mount();
    expect(await screen.findByTestId('erd-node-film', {}, { timeout: 5000 })).toBeInTheDocument();
    expect(screen.getByTestId('erd-count')).toHaveTextContent('16 of 16 tables');
    // Columns with their types and key markers.
    const film = screen.getByTestId('erd-node-film');
    expect(within(film).getByText('film_id')).toBeInTheDocument();
    expect(within(film).getByText('varchar(255)')).toBeInTheDocument();
    expect(within(film).getAllByLabelText('FK').length).toBe(2);
    await waitFor(() =>
      expect(document.querySelectorAll('[data-testid^="erd-edge-"]').length).toBe(25),
    );
  });

  it('searches by name and focuses "table + 1 level"', async () => {
    const user = userEvent.setup();
    await mount();
    await screen.findByTestId('erd-node-film', {}, { timeout: 5000 });
    // Search marks matches and dims the rest.
    await user.type(screen.getByRole('textbox', { name: 'Find a table' }), 'film');
    await waitFor(() =>
      expect(screen.getByTestId('erd-node-film').className).toMatch(/ring-warning/),
    );
    expect(screen.getByTestId('erd-node-customer').className).toMatch(/opacity-35/);
    await user.clear(screen.getByRole('textbox', { name: 'Find a table' }));

    // Focus needs a selected table.
    const focusButton = screen.getByRole('button', { name: /Table \+ neighbours/ });
    expect(focusButton).toBeDisabled();
    fireEvent.click(screen.getByTestId('erd-node-film'));
    await waitFor(() => expect(focusButton).toBeEnabled());
    await user.click(focusButton);
    await waitFor(() =>
      expect(screen.getByTestId('erd-count')).toHaveTextContent('6 of 16 tables'),
    );
    expect(screen.queryByTestId('erd-node-customer')).not.toBeInTheDocument();
    for (const n of ['film', 'language', 'film_actor', 'film_category', 'film_text', 'inventory']) {
      expect(screen.getByTestId(`erd-node-${n}`)).toBeInTheDocument();
    }
    await user.click(screen.getByRole('button', { name: 'Show all tables' }));
    await waitFor(() =>
      expect(screen.getByTestId('erd-count')).toHaveTextContent('16 of 16 tables'),
    );
  });

  it('saves the layout and reopens it with the same positions; dropped tables are flagged', async () => {
    const user = userEvent.setup();
    const first = await mount();
    await screen.findByTestId('erd-node-film', {}, { timeout: 5000 });
    await user.click(screen.getByRole('button', { name: 'Save layout' }));
    const input = await screen.findByRole('textbox', { name: 'Name' });
    expect(input).toHaveValue('app_dev diagram');
    await user.clear(input);
    await user.type(input, 'Main');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());

    const [summary] = await ipc.erdLayoutsList(PG_CONNECTION);
    expect(summary).toMatchObject({ name: 'Main', tableCount: 16 });
    const saved = await ipc.erdLayoutGet(summary!.id);
    const savedTables = parseLayoutTables(saved.tables);
    expect(savedTables).toHaveLength(16);
    const filmSaved = savedTables.find((t) => t.path.segments.at(-1)!.name === 'film')!;
    first.unmount();

    // Move `film` in the stored layout, add a table that does not exist any more and drop a
    // snapshot column: the reopened tab must use the stored positions and flag the differences.
    const ghostPath: ObjectPath = { segments: [{ kind: 'table', name: 'dropped_table' }] };
    const edited: ErdLayout = {
      ...saved,
      tables: [
        ...savedTables.map((t) =>
          tableKey(t.path) === tableKey(filmSaved.path) ? { ...t, x: 4321, y: 1234 } : t,
        ),
        { path: ghostPath, x: 10, y: 10, collapsed: false },
      ],
      snapshot: {
        ...(saved.snapshot as Record<string, unknown>),
        [tableKey(filmSaved.path)]: [['film_id', 'integer']],
      },
    };
    await ipc.erdLayoutSave(edited);

    const spy = vi.spyOn(ipc, 'erdLayoutGet');
    await mount(saved.id);
    await screen.findByTestId('erd-node-film', {}, { timeout: 5000 });
    expect(spy).toHaveBeenCalled();
    expect(screen.getByTestId('erd-node-dropped_table')).toHaveAttribute('data-status', 'missing');
    expect(screen.getByTestId('erd-node-film')).toHaveAttribute('data-status', 'changed');
    expect(screen.getByTestId('erd-diff-banner')).toHaveTextContent(
      'Saved tables that no longer exist: 1',
    );
    expect(screen.getByTestId('erd-diff-banner')).toHaveTextContent(
      'Tables changed since the layout was saved: 1',
    );
    // Stored positions are used: the node wrapper carries the transform.
    const wrapper = screen.getByTestId('erd-node-film').closest('.react-flow__node') as HTMLElement;
    expect(wrapper.style.transform).toContain('translate(4321px, 1234px)');
    expect(screen.getByText('Main')).toBeInTheDocument();
  });

  it('shows the DDL of the selected table', async () => {
    const user = userEvent.setup();
    await mount();
    await screen.findByTestId('erd-node-film', {}, { timeout: 5000 });
    const ddl = vi.spyOn(ipc, 'ddlGenerate');
    fireEvent.click(screen.getByTestId('erd-node-film'));
    await user.click(await screen.findByRole('button', { name: 'DDL' }));
    expect(await screen.findByTestId('erd-ddl-panel')).toBeInTheDocument();
    await waitFor(() => expect(ddl).toHaveBeenCalled());
    const request = ddl.mock.calls[0]![1];
    expect(request.scope.segments.at(-1)).toEqual({ kind: 'table', name: 'film' });
  });

  it('reports a schema that cannot be loaded', async () => {
    sessionId ||= await openMockSession();
    vi.spyOn(ipc, 'schemaModel').mockRejectedValue({ kind: 'unsupported', message: 'no' });
    renderWithProviders(
      <ErdTab sessionId={sessionId} connectionId={PG_CONNECTION} scope={scope} layoutId={null} />,
    );
    expect(
      await screen.findByText('Diagrams are not supported for this database.'),
    ).toBeInTheDocument();
  });
});
