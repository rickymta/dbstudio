import { render, waitFor } from '@testing-library/react';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import { polyfillLayoutApis } from '../data/testUtils';
import { AdviceFocusContext } from './adviceContext';
import { PlanGraph } from './PlanGraph';
import { SAMPLES, TREES } from './planTestData';
import type { PlanNode } from '@/lib/types';

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
  if (!('DOMMatrixReadOnly' in globalThis)) {
    class Matrix {
      m22 = 1;
      constructor(public transform?: string) {}
    }
    (globalThis as unknown as { DOMMatrixReadOnly: unknown }).DOMMatrixReadOnly = Matrix;
  }
});
afterEach(() => vi.restoreAllMocks());

describe('advice focus flash', () => {
  it('flashes nodes of its own diagram, not those of another plan tab with the same ids', async () => {
    const tree = TREES[SAMPLES.pg.file]!;
    const second = tree.root.children[0]!.id;
    const flashed: Element[] = [];
    (HTMLElement.prototype as unknown as { animate: unknown }).animate = function (
      this: HTMLElement,
    ) {
      flashed.push(this);
      return {};
    };
    const ui = (focus: { seq: number; nodeIds: number[] } | null) => (
      <Providers>
        {/* Another tab, mounted first, has the same node ids and no focus. */}
        <div data-testid="other" style={{ width: 800, height: 600 }}>
          <PlanGraph tree={structuredClone(tree)} selectedId={null} onSelect={() => {}} />
        </div>
        <AdviceFocusContext.Provider value={focus}>
          <div data-testid="mine" style={{ width: 800, height: 600 }}>
            <PlanGraph tree={structuredClone(tree)} selectedId={tree.root.id} onSelect={() => {}} />
          </div>
        </AdviceFocusContext.Provider>
      </Providers>
    );
    const view = render(ui(null));
    const mine = view.getByTestId('mine');
    const other = view.getByTestId('other');
    await waitFor(() =>
      expect(mine.querySelectorAll('.react-flow__node').length).toBeGreaterThan(1),
    );
    view.rerender(ui({ seq: 1, nodeIds: [tree.root.id, second] }));
    await waitFor(() => expect(flashed.length).toBeGreaterThan(0));
    expect(flashed.every((el) => mine.contains(el))).toBe(true);
    expect(flashed.some((el) => other.contains(el))).toBe(false);
  });
});
