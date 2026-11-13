import { describe, expect, it, vi } from 'vitest';
import { buildGraph } from './graphModel';
import { flatten } from './planModel';
import { SAMPLES, TREES } from './planTestData';

const tree = TREES[SAMPLES.pg.file]!;
const layout = {
  positions: new Map(flatten(tree.root).map((n) => [n.id, { x: n.id * 10, y: n.id }])),
  width: 100,
  height: 100,
};
const handlers = { selectedId: 3, onSelect: vi.fn(), onToggle: vi.fn(), onHover: vi.fn() };

describe('buildGraph', () => {
  it('makes one card per node and one edge per parent-child pair', () => {
    const { nodes, edges } = buildGraph(tree, layout, new Set(), handlers);
    expect(nodes).toHaveLength(tree.nodeCount);
    expect(edges).toHaveLength(tree.nodeCount - 1);
    expect(nodes.filter((n) => n.data.selected).map((n) => n.id)).toEqual(['3']);
  });

  it('marks the cards and edges of the hot path', () => {
    const { nodes, edges } = buildGraph(tree, layout, new Set(), handlers);
    const hot = new Set(tree.hotPath.map(String));
    expect(
      nodes
        .filter((n) => n.data.hot)
        .map((n) => n.id)
        .sort(),
    ).toEqual([...hot].sort());
    const hotEdges = edges.filter((e) => e.className === 'plan-edge-hot');
    expect(hotEdges).toHaveLength(tree.hotPath.length - 1);
    for (const e of hotEdges) expect(hot.has(e.source) && hot.has(e.target)).toBe(true);
    expect(hotEdges.every((e) => e.style?.strokeWidth === 3)).toBe(true);
  });

  it('hides the descendants of a collapsed node', () => {
    const { nodes, edges } = buildGraph(tree, layout, new Set([2]), handlers);
    expect(nodes.map((n) => n.id)).toEqual(['0', '1', '2']);
    expect(edges).toHaveLength(2);
    expect(nodes.find((n) => n.id === '2')!.data.collapsed).toBe(true);
  });

  it('flags warnings and uses cost shares for estimate-only plans', () => {
    const spill = TREES['oracle_04_group_by_spill_actual.json']!;
    const l = {
      positions: new Map(flatten(spill.root).map((n) => [n.id, { x: 0, y: 0 }])),
      width: 0,
      height: 0,
    };
    const { nodes } = buildGraph(spill, l, new Set(), handlers);
    expect(nodes.find((n) => n.id === '1')!.data.severity).toBe('critical');
    expect(nodes.find((n) => n.id === '2')!.data.severity).toBeNull();
  });
});
