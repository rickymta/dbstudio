// Automatic placement of the tables with ELK (layered algorithm). In the app ELK runs in a Web
// Worker so a large schema never blocks the UI; where workers do not exist (tests) the bundled
// build runs in-process.

import type { ELK as ElkInstance, ElkNode } from 'elkjs/lib/elk-api';
import { type LayoutOptions, type Point, type ViewTable, tableKey } from './model';
import { nodeSize } from './geometry';
import type { Relation } from '@/lib/types';

const WORKER_TIMEOUT_MS = 15_000;

let workerElk: Promise<ElkInstance> | null = null;
let bundledElk: Promise<ElkInstance> | null = null;

/** ELK in a Web Worker (keeps a large layout off the UI thread). */
function inWorker(): Promise<ElkInstance> {
  workerElk ??= import('elkjs/lib/elk-api').then(
    ({ default: ELK }) =>
      new ELK({
        workerUrl: new URL('elkjs/lib/elk-worker.min.js', import.meta.url).href,
      }),
  );
  return workerElk;
}

/** ELK in this thread: used where workers do not exist (tests) and as a fallback. */
function inProcess(): Promise<ElkInstance> {
  bundledElk ??= import('elkjs/lib/elk.bundled.js').then(({ default: ELK }) => new ELK());
  return bundledElk;
}

/** Above this many tables ELK is not run on the UI thread as a fallback (the grid is used). */
const IN_PROCESS_LIMIT = 150;

async function runLayout(graph: ElkNode): Promise<ElkNode> {
  if (typeof Worker !== 'undefined') {
    let instance: ElkInstance | null = null;
    try {
      instance = await inWorker();
      let timer: ReturnType<typeof setTimeout> | undefined;
      const timeout = new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error('elk worker timeout')), WORKER_TIMEOUT_MS);
      });
      try {
        return await Promise.race([instance.layout(graph), timeout]);
      } finally {
        clearTimeout(timer);
      }
    } catch {
      // Stop the stuck worker; never keep a worker that may still be burning CPU.
      instance?.terminateWorker();
      workerElk = null;
      // A big graph must not freeze the UI thread: callers fall back to the grid layout.
      if ((graph.children?.length ?? 0) > IN_PROCESS_LIMIT) {
        throw new Error('layout too large to run outside a worker');
      }
    }
  }
  return (await inProcess()).layout(graph);
}

export interface AutoLayoutInput {
  tables: readonly ViewTable[];
  relations: readonly Relation[];
  options: Pick<LayoutOptions, 'showTypes'>;
  /** Tables drawn collapsed (header only). */
  collapsed?: ReadonlySet<string>;
}

/** Layered left-to-right layout; connected components are packed next to each other. */
export async function autoLayout(input: AutoLayoutInput): Promise<Map<string, Point>> {
  const result = new Map<string, Point>();
  if (input.tables.length === 0) return result;
  const known = new Set(input.tables.map((t) => t.key));
  const seen = new Set<string>();
  const edges: ElkNode['edges'] = [];
  for (const r of input.relations) {
    const a = tableKey(r.fromTable);
    const b = tableKey(r.toTable);
    if (a === b || !known.has(a) || !known.has(b)) continue;
    // One edge per table pair is enough for placement.
    const id = `${a}\u0000${b}`;
    if (seen.has(id)) continue;
    seen.add(id);
    edges.push({ id: `e${edges.length}`, sources: [a], targets: [b] });
  }
  const graph: ElkNode = {
    id: 'root',
    layoutOptions: {
      'elk.algorithm': 'layered',
      'elk.direction': 'RIGHT',
      'elk.spacing.nodeNode': '36',
      'elk.layered.spacing.nodeNodeBetweenLayers': '90',
      'elk.spacing.componentComponent': '60',
      'elk.layered.crossingMinimization.strategy': 'LAYER_SWEEP',
      'elk.layered.considerModelOrder.strategy': 'NODES_AND_EDGES',
      'elk.aspectRatio': '1.6',
    },
    children: input.tables.map((t) => ({
      id: t.key,
      ...nodeSize(t, input.collapsed?.has(t.key) ?? false, input.options),
    })),
    edges,
  };
  const laid = await runLayout(graph);
  for (const c of laid.children ?? []) {
    result.set(c.id, { x: c.x ?? 0, y: c.y ?? 0 });
  }
  return result;
}

/** Grid placement used when ELK cannot run (it is never worse than overlapping everything). */
export function gridLayout(
  tables: readonly ViewTable[],
  options: Pick<LayoutOptions, 'showTypes'>,
): Map<string, Point> {
  const out = new Map<string, Point>();
  const cols = Math.max(1, Math.ceil(Math.sqrt(tables.length)));
  let x = 0;
  let y = 0;
  let rowHeight = 0;
  tables.forEach((t, i) => {
    const size = nodeSize(t, false, options);
    out.set(t.key, { x, y });
    x += size.width + 60;
    rowHeight = Math.max(rowHeight, size.height);
    if ((i + 1) % cols === 0) {
      x = 0;
      y += rowHeight + 50;
      rowHeight = 0;
    }
  });
  return out;
}
