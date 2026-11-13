import {
  Background,
  Controls,
  MiniMap,
  ReactFlow,
  applyNodeChanges,
  type EdgeTypes,
  type NodeChange,
  type NodeTypes,
} from '@xyflow/react';
import '@xyflow/react/dist/style.css';
import { memo, useCallback } from 'react';
import { CrowsFootEdge, type RelationFlowEdge } from './CrowsFootEdge';
import type { Point, Viewport } from './model';
import { TableNode, type TableFlowNode } from './TableNode';
import { useThemeStore } from '@/stores/theme';

/** Big diagrams only render what is on screen; small ones render everything. */
const VIRTUALIZE_ABOVE = 80;

const nodeTypes: NodeTypes = { table: TableNode };
const edgeTypes: EdgeTypes = { relation: CrowsFootEdge };

interface ErdCanvasProps {
  nodes: TableFlowNode[];
  edges: RelationFlowEdge[];
  /** Initial viewport; the canvas fits its content when absent. */
  viewport: Viewport | null;
  onMove: (key: string, position: Point) => void;
  onSelect: (key: string | null) => void;
}

function ErdCanvasView({ nodes, edges, viewport, onMove, onSelect }: ErdCanvasProps) {
  const mode = useThemeStore((s) => s.mode);
  const onNodesChange = useCallback(
    (changes: NodeChange<TableFlowNode>[]) => {
      // Positions are owned by the parent: apply them to get the final coordinates.
      for (const change of changes) {
        if (change.type === 'position' && change.position) {
          const [moved] = applyNodeChanges([change], nodes).filter((n) => n.id === change.id);
          if (moved) onMove(moved.id, moved.position);
        }
      }
    },
    [nodes, onMove],
  );
  return (
    <ReactFlow
      nodes={nodes}
      edges={edges}
      nodeTypes={nodeTypes}
      edgeTypes={edgeTypes}
      colorMode={mode}
      onNodesChange={onNodesChange}
      onNodeClick={(_, node) => onSelect(node.id)}
      onPaneClick={() => onSelect(null)}
      nodesConnectable={false}
      elementsSelectable
      onlyRenderVisibleElements={nodes.length > VIRTUALIZE_ABOVE}
      minZoom={0.03}
      maxZoom={2}
      {...(viewport ? { defaultViewport: viewport } : { fitView: true })}
      fitViewOptions={{ padding: 0.15 }}
    >
      <Background gap={20} />
      <Controls showInteractive={false} />
      <MiniMap pannable zoomable nodeColor="var(--border)" maskColor="rgba(0,0,0,0.12)" />
    </ReactFlow>
  );
}

export const ErdCanvas = memo(ErdCanvasView);
