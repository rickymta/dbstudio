// Tree layout of a plan with ELK (layered; root on the left, or at the top when vertical).

import type { ELK, ElkNode } from 'elkjs/lib/elk-api';
import type { PlanNode } from '@/lib/types';

export const NODE_WIDTH = 240;
export const NODE_HEIGHT = 76;

export type LayoutDirection = 'RIGHT' | 'DOWN';

export interface PlanLayout {
  positions: Map<number, { x: number; y: number }>;
  width: number;
  height: number;
}

interface Engine {
  layout: ELK['layout'];
}

let worker: Promise<Engine | null> | null = null;
let inline: Promise<Engine> | null = null;

/** ELK in a web worker, so a big plan does not freeze the window; `null` when unavailable. */
function workerEngine(): Promise<Engine | null> {
  worker ??= (async () => {
    if (typeof Worker === 'undefined') return null;
    try {
      const [{ default: Api }, { default: ElkWorker }] = await Promise.all([
        import('elkjs/lib/elk-api'),
        import('elkjs/lib/elk-worker.min.js?worker'),
      ]);
      return new Api({ workerFactory: () => new ElkWorker() });
    } catch {
      return null;
    }
  })();
  return worker;
}

/** ELK running on the main thread (tests, or when workers are not allowed). */
function inlineEngine(): Promise<Engine> {
  inline ??= import('elkjs/lib/elk.bundled.js').then(({ default: Elk }) => new Elk());
  return inline;
}

async function runLayout(graph: ElkNode): Promise<ElkNode> {
  const w = await workerEngine();
  if (w) {
    try {
      return await w.layout(graph);
    } catch {
      // A worker that cannot start (CSP, packaging): use the main thread from now on.
      worker = Promise.resolve(null);
    }
  }
  return (await inlineEngine()).layout(graph);
}

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
  direction: LayoutDirection = 'RIGHT',
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
  const result = await runLayout({
    id: 'root',
    layoutOptions: {
      'elk.algorithm': 'layered',
      'elk.direction': direction,
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
