// Tree layout of a plan with ELK (layered, root on the left, data flowing right to left).

import ELK from 'elkjs/lib/elk.bundled.js';
import type { PlanNode } from '@/lib/types';

export const NODE_WIDTH = 240;
export const NODE_HEIGHT = 76;

export interface PlanLayout {
  positions: Map<number, { x: number; y: number }>;
  width: number;
  height: number;
}

const elk = new ELK();

interface Item {
  id: string;
  width: number;
  height: number;
  children?: Item[];
}

/** Lay out the visible part of the tree (`collapsed` nodes hide their descendants). */
export async function layoutPlan(
  root: PlanNode,
  collapsed: ReadonlySet<number>,
): Promise<PlanLayout> {
  const nodes: Item[] = [];
  const edges: { id: string; sources: string[]; targets: string[] }[] = [];
  const walk = (n: PlanNode) => {
    nodes.push({ id: String(n.id), width: NODE_WIDTH, height: NODE_HEIGHT });
    if (collapsed.has(n.id)) return;
    for (const c of n.children) {
      edges.push({ id: `e${n.id}-${c.id}`, sources: [String(n.id)], targets: [String(c.id)] });
      walk(c);
    }
  };
  walk(root);
  const result = await elk.layout({
    id: 'root',
    layoutOptions: {
      'elk.algorithm': 'layered',
      'elk.direction': 'RIGHT',
      'elk.layered.spacing.nodeNodeBetweenLayers': '70',
      'elk.spacing.nodeNode': '22',
      'elk.layered.nodePlacement.strategy': 'BRANDES_KOEPF',
    },
    children: nodes,
    edges,
  });
  const positions = new Map<number, { x: number; y: number }>();
  for (const c of result.children ?? []) {
    positions.set(Number(c.id), { x: c.x ?? 0, y: c.y ?? 0 });
  }
  let width = 0;
  let height = 0;
  for (const p of positions.values()) {
    width = Math.max(width, p.x + NODE_WIDTH);
    height = Math.max(height, p.y + NODE_HEIGHT);
  }
  return { positions, width, height };
}
