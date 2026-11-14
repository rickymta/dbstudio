import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import { Toaster } from '@/components/ui/Toast';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import type { PlanNode } from '@/lib/types';
import { polyfillLayoutApis } from '../data/testUtils';
import type { LayoutDirection } from './elkLayout';
import { PlanGraph } from './PlanGraph';
import { PlanTab } from './PlanTab';
import { usePlanStore } from './planStore';
import { bigPlanTree, SAMPLES, seedPlan, TREES } from './planTestData';
import { fitViewport, initialViewport, MIN_READABLE_ZOOM } from './viewport';

const layoutCalls: LayoutDirection[] = [];
// A plain grid (rows down, one column per depth; or one row per depth when vertical).
vi.mock('./elkLayout', () => ({
  NODE_WIDTH: 240,
  NODE_HEIGHT: 76,
  layoutPlan: async (root: PlanNode, collapsed: ReadonlySet<number>, direction = 'RIGHT') => {
    layoutCalls.push(direction as LayoutDirection);
    const positions = new Map<number, { x: number; y: number }>();
    let slot = 0;
    const walk = (n: PlanNode, depth: number) => {
      positions.set(
        n.id,
        direction === 'DOWN'
          ? { x: slot++ * 260, y: depth * 120 }
          : { x: depth * 310, y: slot++ * 100 },
      );
      if (!collapsed.has(n.id)) n.children.forEach((c) => walk(c, depth + 1));
    };
    walk(root, 0);
    let width = 0;
    let height = 0;
    for (const p of positions.values()) {
      width = Math.max(width, p.x + 240);
      height = Math.max(height, p.y + 76);
    }
    return { positions, width, height };
  },
}));

beforeAll(() => {
  polyfillLayoutApis();
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
  layoutCalls.length = 0;
  try {
    localStorage.clear();
  } catch {
    /* ignore */
  }
  await i18n.changeLanguage('en');
});
afterEach(() => vi.restoreAllMocks());

/** Give the plan tab a width (jsdom has no layout) and React Flow's container a size. */
function stubLayout(tabWidth: number, view = { width: 900, height: 600 }) {
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (
    this: HTMLElement,
  ) {
    const w = this.dataset.testid === 'plan-tab' ? tabWidth : 0;
    return { x: 0, y: 0, top: 0, left: 0, right: w, bottom: 0, width: w, height: 0 } as DOMRect;
  });
  vi.spyOn(HTMLElement.prototype, 'offsetWidth', 'get').mockReturnValue(view.width);
  vi.spyOn(HTMLElement.prototype, 'offsetHeight', 'get').mockReturnValue(view.height);
}

function mountTab(planId: string) {
  render(
    <Providers>
      <PlanTab planId={planId} sessionId="s1" connectionId="c1" />
    </Providers>,
  );
}

describe('diagram first viewport (F2a)', () => {
  const BOUNDS = { width: 6000, height: 4000 };
  const VIEW = { width: 900, height: 600 };

  it('uses a readable zoom with the root in the top-left corner when a fit is too small', () => {
    expect(fitViewport(BOUNDS, VIEW).zoom).toBeLessThan(MIN_READABLE_ZOOM);
    const root = { x: 0, y: 1800 };
    const vp = initialViewport(BOUNDS, VIEW, root);
    expect(vp.zoom).toBeGreaterThanOrEqual(MIN_READABLE_ZOOM);
    const screenX = root.x * vp.zoom + vp.x;
    const screenY = root.y * vp.zoom + vp.y;
    expect(screenX).toBeGreaterThanOrEqual(0);
    expect(screenX).toBeLessThanOrEqual(VIEW.width - 240 * vp.zoom);
    expect(screenY).toBeGreaterThanOrEqual(0);
    expect(screenY).toBeLessThanOrEqual(VIEW.height - 76 * vp.zoom);
  });

  it('still fits a small plan entirely', () => {
    const small = { width: 700, height: 300 };
    expect(initialViewport(small, VIEW, { x: 0, y: 0 })).toEqual(fitViewport(small, VIEW));
  });

  it('renders a 40+ node plan at a readable zoom', async () => {
    stubLayout(1400);
    const tree = bigPlanTree(44);
    expect(tree.nodeCount).toBeGreaterThan(40);
    render(
      <Providers>
        <div style={{ width: 900, height: 600 }}>
          <PlanGraph tree={tree} selectedId={null} onSelect={() => {}} />
        </div>
      </Providers>,
    );
    await screen.findByTestId('plan-node-0');
    const transform = () =>
      document.querySelector<HTMLElement>('.react-flow__viewport')!.style.transform;
    await waitFor(() => {
      const m = /scale\(([\d.]+)\)/.exec(transform());
      expect(Number(m?.[1])).toBeGreaterThanOrEqual(MIN_READABLE_ZOOM);
    });
    const m = /translate\((-?[\d.]+)px,\s*(-?[\d.]+)px\)\s*scale\(([\d.]+)\)/.exec(transform())!;
    // The root (grid position 0,0) sits inside the viewport.
    expect(Number(m[1])).toBeGreaterThanOrEqual(0);
    expect(Number(m[2])).toBeGreaterThanOrEqual(0);
  });

  it('shows the minimap only for big plans', async () => {
    stubLayout(1400);
    const small = render(
      <Providers>
        <PlanGraph tree={TREES[SAMPLES.pg.file]!} selectedId={null} onSelect={() => {}} />
      </Providers>,
    );
    await screen.findByTestId('plan-node-0');
    expect(document.querySelector('.react-flow__minimap')).toBeNull();
    small.unmount();
    render(
      <Providers>
        <PlanGraph tree={bigPlanTree(44)} selectedId={null} onSelect={() => {}} />
      </Providers>,
    );
    await screen.findByTestId('plan-node-0');
    expect(document.querySelector('.react-flow__minimap')).not.toBeNull();
  });

  it('switches between horizontal and vertical layout and remembers it', async () => {
    const user = userEvent.setup();
    const tree = TREES[SAMPLES.pg.file]!;
    const first = render(
      <Providers>
        <PlanGraph tree={tree} selectedId={null} onSelect={() => {}} />
      </Providers>,
    );
    await screen.findByTestId('plan-node-0');
    expect(layoutCalls.at(-1)).toBe('RIGHT');
    await user.click(screen.getByRole('button', { name: 'Vertical layout' }));
    await waitFor(() => expect(layoutCalls.at(-1)).toBe('DOWN'));
    expect(localStorage.getItem('dbstudio.plan.direction')).toBe('DOWN');
    expect(screen.getByRole('button', { name: 'Horizontal layout' })).toBeInTheDocument();
    first.unmount();
    // A new diagram starts vertical.
    layoutCalls.length = 0;
    render(
      <Providers>
        <PlanGraph tree={tree} selectedId={null} onSelect={() => {}} />
      </Providers>,
    );
    await screen.findByTestId('plan-node-0');
    expect(layoutCalls[0]).toBe('DOWN');
  });
});

describe('plan header (F2b)', () => {
  const MORE = { name: 'More actions' };

  it('puts the four secondary actions in the ⋯ menu below 1100px', async () => {
    stubLayout(1000);
    const user = userEvent.setup();
    mountTab(seedPlan());
    const header = await screen.findByTestId('plan-header');
    expect(header.className).toContain('flex-nowrap');
    expect(within(header).queryByRole('button', { name: 'Save plan file' })).toBeNull();
    // Primary actions stay.
    expect(within(header).getByRole('button', { name: /Estimate only/ })).toBeInTheDocument();
    await user.click(within(header).getByRole('button', MORE));
    const items = await screen.findAllByRole('menuitem');
    expect(items.map((i) => i.textContent)).toEqual([
      'Save plan file',
      'Open plan file',
      'Compare plans',
      'Copy raw plan',
    ]);
    // Planning time and operator count are dropped under 900px only.
    expect(within(header).queryByText('Planning')).not.toBeNull();
  });

  it('drops the secondary stats below 900px', () => {
    stubLayout(800);
    mountTab(seedPlan());
    const header = screen.getByTestId('plan-header');
    expect(within(header).queryByText('Planning')).toBeNull();
    expect(within(header).queryByText('Operators')).toBeNull();
    expect(within(header).getByText('Total cost')).toBeInTheDocument();
  });

  it('uses icon-only buttons between 1100 and 1280px and labelled ones above', () => {
    stubLayout(1200);
    const first = render(
      <Providers>
        <PlanTab planId={seedPlan()} sessionId="s1" connectionId="c1" />
      </Providers>,
    );
    const header = screen.getByTestId('plan-header');
    expect(within(header).queryByRole('button', MORE)).toBeNull();
    const save = within(header).getByRole('button', { name: 'Save plan file' });
    expect(save).not.toHaveTextContent('Save plan file');
    first.unmount();
    vi.restoreAllMocks();
    stubLayout(1400);
    mountTab(seedPlan());
    expect(
      within(screen.getByTestId('plan-header')).getByRole('button', { name: 'Save plan file' }),
    ).toHaveTextContent('Save plan file');
  });
});

describe('inspector and table (F2c)', () => {
  const ACTUAL_COLUMNS = ['Actual rows', 'Loops', 'Self time', 'Share'];

  it('collapses itself in a narrow plan area and keeps all ANALYZE columns visible', async () => {
    stubLayout(1000);
    const user = userEvent.setup();
    mountTab(seedPlan());
    expect(await screen.findByTestId('plan-inspector-collapsed')).toBeInTheDocument();
    expect(screen.queryByTestId('plan-details')).toBeNull();
    await user.click(screen.getByRole('tab', { name: 'Table' }));
    const table = await screen.findByTestId('plan-table');
    for (const name of ACTUAL_COLUMNS) {
      expect(within(table).getByRole('columnheader', { name })).toBeInTheDocument();
    }
    // Object moved to the end, Operator is first.
    const heads = within(table)
      .getAllByRole('columnheader')
      .map((h) => h.textContent);
    expect(heads[0]).toBe('Operator');
    expect(heads.at(-1)).toBe('Object');
    expect(table.className).toContain('min-w-[680px]');
  });

  it('keeps the Operator column sticky when the inspector is opened, and remembers that', async () => {
    stubLayout(1000);
    const user = userEvent.setup();
    mountTab(seedPlan());
    await user.click(screen.getByRole('tab', { name: 'Table' }));
    await user.click(await screen.findByTestId('plan-inspector-collapsed'));
    expect(await screen.findByTestId('plan-details')).toBeInTheDocument();
    expect(localStorage.getItem('dbstudio.plan.inspector')).toBe('open');
    const table = screen.getByTestId('plan-table');
    expect(within(table).getByRole('columnheader', { name: 'Operator' }).className).toContain(
      'sticky',
    );
    const firstCell = within(screen.getByTestId('plan-row-0')).getAllByRole('cell')[0]!;
    expect(firstCell.className).toContain('sticky');
  });

  it('is open in a wide area and collapses and expands on request', async () => {
    stubLayout(1400);
    const user = userEvent.setup();
    mountTab(seedPlan());
    expect(await screen.findByTestId('plan-details')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Collapse the details panel' }));
    expect(screen.getByTestId('plan-inspector-collapsed')).toBeInTheDocument();
    expect(localStorage.getItem('dbstudio.plan.inspector')).toBe('collapsed');
    await user.click(screen.getByRole('button', { name: 'Show the details panel' }));
    expect(screen.getByTestId('plan-details')).toBeInTheDocument();
  });

  it('resizes with the handle within 240-480px', async () => {
    stubLayout(1400);
    mountTab(seedPlan());
    const panel = await screen.findByTestId('plan-inspector');
    const handle = screen.getByRole('separator', { name: 'Resize the details panel' });
    expect(panel.style.width).toBe('300px');
    fireEvent.keyDown(handle, { key: 'ArrowLeft', shiftKey: true });
    expect(panel.style.width).toBe('340px');
    for (let i = 0; i < 10; i++) fireEvent.keyDown(handle, { key: 'ArrowLeft', shiftKey: true });
    expect(panel.style.width).toBe('480px');
    for (let i = 0; i < 20; i++) fireEvent.keyDown(handle, { key: 'ArrowRight', shiftKey: true });
    expect(panel.style.width).toBe('240px');
    expect(localStorage.getItem('dbstudio.plan.inspectorWidth')).toBe('240');
  });

  it('hides the actual-statistics columns for an estimate-only plan', async () => {
    stubLayout(1400);
    const user = userEvent.setup();
    const sample = {
      file: 'mssql_01_clustered_scan_estimated.xml',
      format: 'msSqlShowplanXml',
      text: '',
    } as const;
    const id = seedPlan({}, sample);
    mountTab(id);
    await user.click(screen.getByRole('tab', { name: 'Table' }));
    const table = await screen.findByTestId('plan-table');
    expect(within(table).queryByRole('columnheader', { name: 'Actual rows' })).toBeNull();
    expect(table.className).toContain('min-w-[560px]');
  });
});

describe('lost session in the plan tab (F2d)', () => {
  it('offers to reconnect and explains instead of a generic failure', async () => {
    const user = userEvent.setup();
    const id = seedPlan({
      status: 'error',
      tree: null,
      raw: null,
      error: { kind: 'notFound', message: 'session s1' },
    });
    const reconnect = vi.spyOn(ipc, 'sessionReconnect').mockRejectedValue({
      kind: 'other',
      message: 'boom',
    });
    render(
      <Providers>
        <PlanTab planId={id} sessionId="s1" connectionId="c1" />
        <Toaster />
      </Providers>,
    );
    expect(screen.getByText('The connection session was lost')).toBeInTheDocument();
    expect(screen.queryByText('The statement could not be explained')).toBeNull();
    await act(async () => {
      await user.click(screen.getByRole('button', { name: 'Reconnect' }));
    });
    expect(reconnect).toHaveBeenCalledWith('s1');
    expect(await screen.findByText('boom')).toBeInTheDocument();
  });
});
