import { render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { polyfillLayoutApis } from '../data/testUtils';
import { comparePlans } from './compareModel';
import type { PlanLayout } from './elkLayout';
import { PlanCompare } from './PlanCompare';
import {
  exportPlan,
  planToSvg,
  svgToPngBase64,
  themeColors,
  toHex,
  utf8ToBase64,
} from './planExport';
import { flatten } from './planModel';
import PlanRaw from './PlanRaw';
import { PlanTable } from './PlanTable';
import { usePlanStore } from './planStore';
import { rawOf, SAMPLES, seedPlan, TREES } from './planTestData';

const pg = TREES[SAMPLES.pg.file]!;
const pgEst = TREES['pg_08_cte_subplan.json']!;
const layoutOf = (tree: typeof pg): PlanLayout => ({
  positions: new Map(flatten(tree.root).map((n) => [n.id, { x: n.id * 260, y: n.id * 90 }])),
  width: 1600,
  height: 600,
});

beforeAll(() => polyfillLayoutApis());
beforeEach(async () => {
  usePlanStore.setState({ entries: {} });
  await i18n.changeLanguage('en');
});
afterEach(() => {
  vi.restoreAllMocks();
  document.documentElement.removeAttribute('style');
});

describe('SVG export', () => {
  it('writes real text for every node and no foreignObject', () => {
    const svg = planToSvg(pg, layoutOf(pg), themeColors());
    expect(svg.startsWith('<svg xmlns="http://www.w3.org/2000/svg"')).toBe(true);
    expect(svg).not.toContain('foreignObject');
    for (const n of flatten(pg.root)) {
      expect(svg).toContain(`data-node="${n.id}"`);
      expect(svg).toContain(`>${n.op}</text>`);
    }
    expect(svg.match(/<path /g)).toHaveLength(pg.nodeCount - 1);
    // Well-formed XML.
    const doc = new DOMParser().parseFromString(svg, 'image/svg+xml');
    expect(doc.querySelector('parsererror')).toBeNull();
  });

  it('follows the active theme and escapes text', () => {
    document.documentElement.style.setProperty('--surface', '#101820');
    document.documentElement.style.setProperty('--background', '#050505');
    const dark = planToSvg(pg, layoutOf(pg), themeColors());
    expect(dark).toContain('fill="#101820"');
    expect(dark).toContain('fill="#050505"');
    document.documentElement.style.setProperty('--surface', '#ffffff');
    expect(planToSvg(pg, layoutOf(pg), themeColors())).toContain('fill="#ffffff"');
    const tricky = structuredClone(pg);
    tricky.root.op = 'A<B & "C"';
    expect(planToSvg(tricky, layoutOf(tricky), themeColors())).toContain(
      'A&lt;B &amp; &quot;C&quot;',
    );
  });

  it('highlights the hot path edges', () => {
    const svg = planToSvg(pg, layoutOf(pg), { ...themeColors(), accent: '#abcdef' });
    expect(svg.match(/stroke="#abcdef" stroke-width="3"/g)).toHaveLength(pg.hotPath.length - 1);
  });

  it('saves SVG through the save dialog command', async () => {
    const save = vi.spyOn(ipc, 'fileSaveBytes').mockResolvedValue('/tmp/plan.svg');
    expect(await exportPlan(pg, layoutOf(pg), 'svg', 'q1')).toBe('/tmp/plan.svg');
    const [name, b64, filters] = save.mock.calls[0]!;
    expect(name).toBe('q1.svg');
    expect(filters[0]!.extensions).toEqual(['svg']);
    expect(atob(b64)).toContain('<svg');
  });

  it('rasterises to PNG through a canvas', async () => {
    class FakeImage {
      onload: (() => void) | null = null;
      onerror: (() => void) | null = null;
      set src(_: string) {
        queueMicrotask(() => this.onload?.());
      }
    }
    vi.stubGlobal('Image', FakeImage);
    URL.createObjectURL ??= () => 'blob:x';
    URL.revokeObjectURL ??= () => {};
    const drawn: number[][] = [];
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue({
      drawImage: (_i: unknown, _x: number, _y: number, w: number, h: number) => drawn.push([w, h]),
    } as unknown as CanvasRenderingContext2D);
    vi.spyOn(HTMLCanvasElement.prototype, 'toBlob').mockImplementation((cb) =>
      cb(new Blob([new Uint8Array([137, 80, 78, 71])], { type: 'image/png' })),
    );
    const b64 = await svgToPngBase64('<svg/>', 100, 50, 2);
    expect(atob(b64)).toBe('\x89PNG');
    expect(drawn).toEqual([[200, 100]]);
    vi.unstubAllGlobals();
  });

  it('encodes UTF-8 to base64', () => {
    expect(atob(utf8ToBase64('abc'))).toBe('abc');
    expect(utf8ToBase64('é')).toBe('w6k=');
  });
});

describe('SVG colors and text', () => {
  it('converts modern CSS colors to hex through the canvas', () => {
    expect(toHex('#abc')).toBe('#abc');
    // jsdom has no canvas: unresolved colors are returned as they are.
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue(null);
    expect(toHex('oklch(0.5 0.1 200)')).toBe('oklch(0.5 0.1 200)');
    vi.restoreAllMocks();
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockImplementation(function (
      this: HTMLCanvasElement,
    ) {
      return {
        canvas: this,
        fillStyle: '',
        clearRect() {},
        fillRect() {},
        getImageData: () => ({ data: [255, 128, 0, 255] }),
      } as unknown as CanvasRenderingContext2D;
    });
    expect(toHex('oklch(0.7 0.2 50)')).toBe('#ff8000');
    document.documentElement.style.setProperty('--accent', 'oklch(0.55 0.2 262)');
    expect(themeColors().accent).toBe('#ff8000');
    const svg = planToSvg(pg, layoutOf(pg), themeColors());
    expect(svg).not.toContain('oklch');
  });

  it('drops characters XML cannot carry', () => {
    const t = structuredClone(pg);
    t.root.op = 'A\u0001B\u001fC\u0000D\ufffeE';
    const svg = planToSvg(t, layoutOf(t), themeColors());
    expect(svg).toContain('>A B C D E</text>');
    const doc = new DOMParser().parseFromString(svg, 'image/svg+xml');
    expect(doc.querySelector('parsererror')).toBeNull();
  });
});

describe('comparePlans', () => {
  it('compares totals and matches nodes by operator and object', () => {
    const same = comparePlans(pg, structuredClone(pg));
    expect(same.metric).toBe('time');
    expect(same.nodes.every((n) => n.a && n.b && n.delta === 0)).toBe(true);
    expect(same.totals.find((t) => t.key === 'executionTimeMs')!.ratio).toBe(1);

    const faster = structuredClone(pg);
    faster.totals.executionTimeMs = 17.348;
    flatten(faster.root).forEach((n) => {
      if (n.op === 'Seq Scan' && n.object?.name === 'orders') n.selfTimeMs = 1;
    });
    const r = comparePlans(pg, faster);
    expect(r.totals.find((t) => t.key === 'executionTimeMs')!.ratio).toBeCloseTo(0.5);
    expect(r.nodes[0]!.signature).toContain('Seq Scan|orders');
    expect(r.nodes[0]!.delta).toBeLessThan(0);
  });

  it('lists nodes that exist on one side only, and falls back to cost', () => {
    const r = comparePlans(pg, pgEst);
    expect(r.metric).toBe('cost');
    expect(r.nodes.some((n) => n.a && !n.b)).toBe(true);
    expect(r.nodes.some((n) => !n.a && n.b)).toBe(true);
    const estEst = comparePlans(pgEst, structuredClone(pgEst));
    expect(estEst.metric).toBe('cost');
  });
});

describe('PlanCompare dialog', () => {
  function open(id: string) {
    render(
      <Providers>
        <PlanCompare planId={id} open onOpenChange={() => {}} />
      </Providers>,
    );
  }

  it('says so when there is nothing to compare with', () => {
    open(seedPlan());
    expect(screen.getByText(/Open another plan/)).toBeInTheDocument();
  });

  it('shows totals and node differences against another open plan', async () => {
    const a = seedPlan();
    seedPlan({
      statement: 'select 2',
      tree: structuredClone(TREES['mysql_tree_01_analyze_join.txt']!),
    });
    open(a);
    const totals = within(await screen.findByTestId('plan-compare-totals'));
    expect(totals.getByText('Execution time')).toBeInTheDocument();
    const nodes = within(screen.getByTestId('plan-compare-nodes'));
    expect(
      nodes.getAllByText('In both').length + nodes.getAllByText('Only in this plan').length,
    ).toBeGreaterThan(0);
    expect(nodes.getAllByText('Only in the other plan').length).toBeGreaterThan(0);
  });
});

describe('operator icons', () => {
  it('shows an icon per operator kind in the table', () => {
    render(
      <Providers>
        <PlanTable
          tree={pg}
          selectedId={null}
          onSelect={() => {}}
          collapsed={new Set()}
          onToggle={() => {}}
        />
      </Providers>,
    );
    const kinds = new Set(flatten(pg.root).map((n) => n.kind));
    for (const k of kinds) {
      expect(document.querySelector(`svg[data-kind="${k}"]`), k).not.toBeNull();
    }
  });
});

describe('raw XML', () => {
  it('opens a SQL Server plan with XML highlighting and the node marked', async () => {
    render(
      <Providers>
        <PlanRaw raw={rawOf(SAMPLES.mssql.format, SAMPLES.mssql.text)} range={[10, 12]} />
      </Providers>,
    );
    const host = screen.getByTestId('plan-raw');
    expect(host).toHaveAttribute('data-selected-lines', '10-12');
    await waitFor(() => expect(host.querySelectorAll('.cm-line').length).toBeGreaterThan(0));
    await waitFor(() => expect(host.querySelectorAll('.cm-plan-selected').length).toBe(3));
    // lang-xml tokenises the document: tag names carry a highlight class.
    await waitFor(() =>
      expect(
        host.querySelector('.cm-content')!.querySelectorAll('span[class]').length,
      ).toBeGreaterThan(0),
    );
  });
});
