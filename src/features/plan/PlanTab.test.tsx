import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import type { PlanNode, PlanResult } from '@/lib/types';
import { polyfillLayoutApis } from '../data/testUtils';
import { locateRaw, nodeById, flatten } from './planModel';
import { PlanTab } from './PlanTab';
import { usePlanStore } from './planStore';
import { rawOf, SAMPLES, seedPlan, TREES } from './planTestData';

// ELK is slow and asynchronous; a plain grid is enough to exercise the diagram.
vi.mock('./elkLayout', () => ({
  NODE_WIDTH: 240,
  NODE_HEIGHT: 76,
  layoutPlan: async (root: PlanNode, collapsed: ReadonlySet<number>) => {
    const positions = new Map<number, { x: number; y: number }>();
    let row = 0;
    const walk = (n: PlanNode, depth: number) => {
      positions.set(n.id, { x: depth * 300, y: row++ * 100 });
      if (!collapsed.has(n.id)) n.children.forEach((c) => walk(c, depth + 1));
    };
    walk(root, 0);
    return { positions, width: 1200, height: row * 100 };
  },
}));

beforeAll(() => {
  polyfillLayoutApis();
  // React Flow reads the viewport transform through DOMMatrixReadOnly.
  if (!('DOMMatrixReadOnly' in globalThis)) {
    class Matrix {
      m22 = 1;
      constructor(public transform?: string) {}
    }
    (globalThis as unknown as { DOMMatrixReadOnly: unknown }).DOMMatrixReadOnly = Matrix;
  }
});
beforeEach(async () => {
  usePlanStore.setState({ entries: {} });
  await i18n.changeLanguage('en');
});
afterEach(() => vi.restoreAllMocks());

function mount(planId: string) {
  const user = userEvent.setup();
  render(
    <Providers>
      <PlanTab planId={planId} sessionId="s1" connectionId="c1" />
    </Providers>,
  );
  return user;
}

const selected = (id: string) => usePlanStore.getState().entries[id]!.selectedId;
const tree = TREES[SAMPLES.pg.file]!;

describe('PlanTab views', () => {
  it('shows totals, format and the analyzed state in the header', async () => {
    const id = seedPlan();
    mount(id);
    expect(screen.getByText('Actual statistics')).toBeInTheDocument();
    expect(screen.getByText('Executed and rolled back')).toBeInTheDocument();
    expect(screen.getByText(/PostgreSQL JSON/)).toBeInTheDocument();
    expect(screen.getByText('34.7 ms')).toBeInTheDocument();
    expect(await screen.findByTestId('plan-graph')).toBeInTheDocument();
  });

  it('selecting a row in the table selects the node everywhere', async () => {
    const id = seedPlan();
    const user = mount(id);
    await user.click(screen.getByRole('tab', { name: 'Table' }));
    await user.click(await screen.findByTestId('plan-row-3'));
    expect(selected(id)).toBe(3);
    expect(screen.getByTestId('plan-row-3')).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByTestId('plan-row-2')).toHaveAttribute('aria-selected', 'false');
    // The details panel describes it.
    const facts = within(screen.getByTestId('plan-details'));
    expect(facts.getByText('Seq Scan')).toBeInTheDocument();
    expect(facts.getByText('orders (o)')).toBeInTheDocument();

    // The diagram marks the same node ...
    await user.click(screen.getByRole('tab', { name: 'Diagram' }));
    const card = await screen.findByTestId('plan-node-3');
    expect(card).toHaveAttribute('aria-pressed', 'true');
    expect(screen.getByTestId('plan-node-2')).toHaveAttribute('aria-pressed', 'false');

    // ... and the raw text marks its lines.
    await user.click(screen.getByRole('tab', { name: 'Raw' }));
    const raw = await screen.findByTestId('plan-raw');
    const expected = locateRaw(usePlanStore.getState().entries[id]!.raw!, nodeById(tree, 3)!)!;
    expect(raw).toHaveAttribute('data-selected-lines', `${expected[0]}-${expected[1]}`);
    await waitFor(() =>
      expect(raw.querySelectorAll('.cm-plan-selected').length).toBeGreaterThan(0),
    );
  });

  it('clicking a diagram node updates the table selection', async () => {
    const id = seedPlan();
    const user = mount(id);
    // fireEvent: the pointer-down of userEvent reaches d3-zoom, which needs a real `view`.
    fireEvent.click(await screen.findByTestId('plan-node-4'));
    expect(selected(id)).toBe(4);
    await user.click(screen.getByRole('tab', { name: 'Table' }));
    expect(await screen.findByTestId('plan-row-4')).toHaveAttribute('aria-selected', 'true');
  });

  it('moves the selection with the arrow keys in the table', async () => {
    const id = seedPlan();
    const user = mount(id);
    await user.click(screen.getByRole('tab', { name: 'Table' }));
    await user.click(await screen.findByTestId('plan-row-1'));
    await user.keyboard('{ArrowDown}');
    expect(selected(id)).toBe(2);
    await user.keyboard('{ArrowUp}{ArrowUp}');
    expect(selected(id)).toBe(0);
  });

  it('marks the hot path in the diagram and the table', async () => {
    const id = seedPlan();
    const user = mount(id);
    const hot = new Set(tree.hotPath);
    expect(hot.size).toBeGreaterThan(1);
    for (const n of flatten(tree.root)) {
      const card = await screen.findByTestId(`plan-node-${n.id}`);
      expect(card.hasAttribute('data-hot')).toBe(hot.has(n.id));
    }
    await user.click(screen.getByRole('tab', { name: 'Table' }));
    for (const n of flatten(tree.root)) {
      expect((await screen.findByTestId(`plan-row-${n.id}`)).hasAttribute('data-hot')).toBe(
        hot.has(n.id),
      );
    }
  });

  it('shows a detailed tooltip while hovering a node', async () => {
    seedPlan();
    const id = Object.keys(usePlanStore.getState().entries)[0]!;
    mount(id);
    const card = await screen.findByTestId('plan-node-2');
    expect(screen.queryByTestId('plan-tooltip')).toBeNull();
    fireEvent.mouseEnter(card);
    const tip = await screen.findByTestId('plan-tooltip');
    expect(within(tip).getByText('Hash Join')).toBeInTheDocument();
    expect(within(tip).getByText(/Hash Cond/)).toBeInTheDocument();
    fireEvent.mouseLeave(card);
    expect(screen.queryByTestId('plan-tooltip')).toBeNull();
  });

  it('collapses a subtree in the table and the diagram', async () => {
    const id = seedPlan();
    const user = mount(id);
    await user.click(screen.getByRole('tab', { name: 'Table' }));
    await user.click(
      within(await screen.findByTestId('plan-row-2')).getByRole('button', {
        name: 'Collapse children',
      }),
    );
    expect(screen.queryByTestId('plan-row-3')).toBeNull();
    expect(screen.getByTestId('plan-row-2')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Expand all' }));
    expect(await screen.findByTestId('plan-row-3')).toBeInTheDocument();
  });
});

describe('warnings', () => {
  it('lists the warnings and jumps to the node of one', async () => {
    const sample = SAMPLES.oracle;
    const id = seedPlan({}, sample);
    const user = mount(id);
    const details = within(await screen.findByTestId('plan-details'));
    expect(details.getByText(/Warnings: 4/)).toBeInTheDocument();
    await user.click(details.getByText(/Spilled to disk/));
    expect(selected(id)).toBe(1);
    // Warning nodes carry an icon in the table.
    await user.click(screen.getByRole('tab', { name: 'Table' }));
    const row = await screen.findByTestId('plan-row-1');
    expect(within(row).getAllByLabelText(/Warning|Critical/).length).toBeGreaterThan(0);
  });
});

describe('running again', () => {
  const answer = (): PlanResult => ({
    raw: rawOf(SAMPLES.pg.format, SAMPLES.pg.text),
    tree: structuredClone(tree),
    parseError: null,
  });

  it('asks before analyzing a statement that changes data', async () => {
    const spy = vi.spyOn(ipc, 'planExplain').mockResolvedValue(answer());
    const id = seedPlan({
      statement: 'UPDATE orders SET total = 0',
      request: {
        queryId: 'q1',
        text: 'UPDATE orders SET total = 0',
        analyze: false,
        options: {},
      },
    });
    const user = mount(id);
    await user.click(screen.getByRole('button', { name: /Run with ANALYZE/ }));
    expect(spy).not.toHaveBeenCalled();
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText(/changes data/)).toBeInTheDocument();
    await user.click(within(dialog).getByRole('button', { name: 'Run with ANALYZE' }));
    await waitFor(() => expect(spy).toHaveBeenCalledTimes(1));
    expect(spy.mock.calls[0]![2].analyze).toBe(true);
  });

  it('analyzes a plain query without asking, and goes back to estimates', async () => {
    const spy = vi.spyOn(ipc, 'planExplain').mockResolvedValue(answer());
    const id = seedPlan({
      request: { queryId: 'q1', text: 'select 1', analyze: false, options: {} },
      statement: 'select 1',
    });
    const user = mount(id);
    await user.click(screen.getByRole('button', { name: /Run with ANALYZE/ }));
    await waitFor(() => expect(spy).toHaveBeenCalledTimes(1));
    expect(screen.queryByRole('dialog')).toBeNull();
    await user.click(await screen.findByRole('button', { name: /Estimate only/ }));
    await waitFor(() => expect(spy).toHaveBeenCalledTimes(2));
    expect(spy.mock.calls[1]![2].analyze).toBe(false);
  });

  it('has no run buttons for a plan opened from a file', () => {
    const id = seedPlan({ request: null });
    mount(id);
    expect(screen.queryByRole('button', { name: /Run again/ })).toBeNull();
    expect(screen.getByRole('button', { name: /Save plan file/ })).toBeEnabled();
  });

  it('saves the raw plan through the file dialog', async () => {
    const save = vi.spyOn(ipc, 'planFileSave').mockResolvedValue('/tmp/p.dbplan.json');
    const id = seedPlan();
    const user = mount(id);
    await user.click(screen.getByRole('button', { name: /Save plan file/ }));
    await waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    expect(save.mock.calls[0]![0].text).toBe(SAMPLES.pg.text);
  });
});

describe('states', () => {
  it('shows progress while loading', () => {
    const id = seedPlan({ status: 'loading', raw: null, tree: null });
    mount(id);
    expect(screen.getByText('Explaining the statement...')).toBeInTheDocument();
  });

  it('explains unsupported analyze and offers to retry', async () => {
    const id = seedPlan({
      status: 'error',
      raw: null,
      tree: null,
      error: { kind: 'unsupported', message: 'x', capability: 'explain_analyze_writes' },
    });
    mount(id);
    expect(screen.getByText('The statement could not be explained')).toBeInTheDocument();
    expect(screen.getByText(/not supported by this server/)).toBeInTheDocument();
    expect(screen.getAllByRole('button', { name: /Run again/ }).length).toBeGreaterThan(0);
  });

  it('falls back to the raw text when the plan cannot be parsed', async () => {
    const id = seedPlan({ tree: null, parseError: 'unexpected token' });
    mount(id);
    expect(screen.getByRole('alert')).toHaveTextContent('unexpected token');
    expect(screen.getByRole('tab', { name: 'Diagram' })).toBeDisabled();
    expect(await screen.findByTestId('plan-raw')).toBeInTheDocument();
  });

  it('copes with a missing plan', () => {
    mount('unknown');
    expect(screen.getByText('No plan loaded')).toBeInTheDocument();
  });

  it('renders in Vietnamese too', async () => {
    await act(() => i18n.changeLanguage('vi'));
    const id = seedPlan();
    mount(id);
    expect(screen.getByText('Số liệu thực tế')).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: 'Sơ đồ' })).toBeInTheDocument();
  });
});
