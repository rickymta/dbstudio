import { BaseEdge, type Edge, type EdgeProps } from '@xyflow/react';
import { memo } from 'react';
import type { EdgeGeometry } from './geometry';
import { type EndKind, endSymbol } from './notation';

export interface RelationEdgeData extends Record<string, unknown> {
  geometry: EdgeGeometry;
  child: EndKind;
  parent: EndKind;
  /** One end of the edge is the selected table. */
  highlight: boolean;
  dim: boolean;
  label: string;
}

export type RelationFlowEdge = Edge<RelationEdgeData, 'relation'>;

function CrowsFootEdgeView({ id, data, selected }: EdgeProps<RelationFlowEdge>) {
  if (!data) return null;
  const { geometry: g } = data;
  const active = selected || data.highlight;
  const color = active ? 'var(--accent)' : 'var(--muted-foreground)';
  const symbols = [
    endSymbol(data.child, g.start.x, g.start.y, g.startDir.x, g.startDir.y),
    endSymbol(data.parent, g.end.x, g.end.y, g.endDir.x, g.endDir.y),
  ];
  return (
    <g data-testid={`erd-edge-${id}`} opacity={data.dim ? 0.2 : 1}>
      <title>{data.label}</title>
      <BaseEdge
        id={id}
        path={g.path}
        interactionWidth={14}
        style={{ stroke: color, strokeWidth: active ? 2 : 1.3 }}
      />
      {symbols.map((s, i) => (
        <g key={i} fill="none" stroke={color} strokeWidth={active ? 2 : 1.3} strokeLinecap="round">
          <path d={s.d} />
          {s.circle && (
            <circle cx={s.circle.cx} cy={s.circle.cy} r={s.circle.r} fill="var(--surface)" />
          )}
        </g>
      ))}
    </g>
  );
}

export const CrowsFootEdge = memo(CrowsFootEdgeView);
