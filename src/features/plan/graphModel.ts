// Pure construction of the React Flow nodes and edges of a plan.

import type { Edge, Node } from '@xyflow/react';
import type { PlanNode, PlanTree, Severity } from '@/lib/types';
import type { PlanLayout } from './elkLayout';
import { formatDuration, maxSeverity, metricOf, nodeShare, warningsByNode } from './planModel';

export interface CardData extends Record<string, unknown> {
  node: PlanNode;
  selected: boolean;
  hot: boolean;
  severity: Severity | null;
  share: number | null;
  metricLabel: string;
  collapsed: boolean;
  onSelect: (id: number) => void;
  onToggle: (id: number) => void;
  onHover: (id: number | null, rect: DOMRect | null) => void;
}

export type CardNode = Node<CardData, 'plan'>;

export interface GraphHandlers {
  selectedId: number | null;
  onSelect: (id: number) => void;
  onToggle: (id: number) => void;
  onHover: (id: number | null, rect: DOMRect | null) => void;
}

/** Cards for the visible nodes and edges between them; edges of the hot path are marked. */
export function buildGraph(
  tree: PlanTree,
  layout: PlanLayout,
  collapsed: ReadonlySet<number>,
  h: GraphHandlers,
): { nodes: CardNode[]; edges: Edge[] } {
  const warnings = warningsByNode(tree);
  const hot = new Set(tree.hotPath);
  const metric = metricOf(tree);
  const nodes: CardNode[] = [];
  const edges: Edge[] = [];
  const walk = (n: PlanNode) => {
    const pos = layout.positions.get(n.id);
    if (!pos) return;
    nodes.push({
      id: String(n.id),
      type: 'plan',
      position: pos,
      draggable: false,
      data: {
        node: n,
        selected: n.id === h.selectedId,
        hot: hot.has(n.id),
        severity: maxSeverity(warnings.get(n.id)),
        share: nodeShare(tree, n),
        metricLabel: metric === 'time' ? formatDuration(n.selfTimeMs) : '',
        collapsed: collapsed.has(n.id),
        onSelect: h.onSelect,
        onToggle: h.onToggle,
        onHover: h.onHover,
      },
    });
    if (collapsed.has(n.id)) return;
    for (const c of n.children) {
      const onHot = hot.has(n.id) && hot.has(c.id);
      edges.push({
        id: `e${n.id}-${c.id}`,
        source: String(n.id),
        target: String(c.id),
        className: onHot ? 'plan-edge-hot' : undefined,
        style: {
          stroke: onHot ? 'var(--accent)' : 'var(--border)',
          strokeWidth: onHot ? 3 : 1.5,
        },
      });
      walk(c);
    }
  };
  walk(tree.root);
  return { nodes, edges };
}
