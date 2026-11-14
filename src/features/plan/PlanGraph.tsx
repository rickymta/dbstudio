import {
  Background,
  Handle,
  MiniMap,
  Panel,
  Position,
  ReactFlow,
  ReactFlowProvider,
  useReactFlow,
  useStore,
  type Edge,
  type NodeProps,
} from '@xyflow/react';
import '@xyflow/react/dist/style.css';
import {
  ChevronLeft,
  ChevronRight,
  ChevronDown,
  ChevronUp,
  Columns3,
  FileCode2,
  Flame,
  ImageDown,
  Maximize,
  Rows3,
  ZoomIn,
  ZoomOut,
} from 'lucide-react';
import {
  createContext,
  memo,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react';
import { useTranslation } from 'react-i18next';
import { IconButton } from '@/components/ui/IconButton';
import { toast } from '@/components/ui/toastStore';
import { toIpcError } from '@/lib/ipc';
import { cn } from '@/lib/cn';
import type { PlanTree, Severity } from '@/lib/types';
import {
  NODE_HEIGHT,
  NODE_WIDTH,
  layoutPlan,
  type LayoutDirection,
  type PlanLayout,
} from './elkLayout';
import { buildGraph, initialCollapsed, type CardNode } from './graphModel';
import { NodeFacts, SeverityIcon } from './PlanDetails';
import { PlanNodeIcon } from './PlanNodeIcon';
import { exportPlan, type ExportFormat } from './planExport';
import { useGraphDirection } from './planPrefs';
import { flatten, formatCount, formatPercent, objectLabel } from './planModel';
import { initialViewport } from './viewport';

/** The MiniMap only helps on plans too big to see at a glance. */
const MINIMAP_MIN_NODES = 25;

/** Layout direction, read by the cards to put their handles and collapse button on the right side. */
const DirectionContext = createContext<LayoutDirection>('RIGHT');

const BORDER: Record<Severity, string> = {
  info: 'border-border',
  warning: 'border-warning',
  critical: 'border-danger',
};

/** One operator in the diagram. */
const PlanNodeCard = memo(function PlanNodeCard({ data }: NodeProps<CardNode>) {
  const { t } = useTranslation('plan');
  const { node, selected, hot, severity, share, metricLabel, collapsed } = data;
  const vertical = useContext(DirectionContext) === 'DOWN';
  const rows = node.actualRows ?? node.estRows;
  return (
    <div
      role="button"
      tabIndex={0}
      aria-pressed={selected}
      data-testid={`plan-node-${node.id}`}
      data-hot={hot || undefined}
      onClick={() => data.onSelect(node.id)}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          data.onSelect(node.id);
        }
      }}
      onMouseEnter={(e) => data.onHover(node.id, e.currentTarget.getBoundingClientRect())}
      onMouseLeave={() => data.onHover(null, null)}
      className={cn(
        // React Flow switches pointer events off for nodes it does not consider interactive.
        'relative cursor-pointer rounded-md border bg-surface px-2.5 py-1.5 text-xs shadow-sm',
        severity ? BORDER[severity] : 'border-border',
        hot && 'border-2 border-accent',
        selected && 'ring-2 ring-ring bg-warning-soft',
      )}
      style={{ width: NODE_WIDTH, height: NODE_HEIGHT, pointerEvents: 'auto' }}
    >
      <Handle
        type="target"
        position={vertical ? Position.Top : Position.Left}
        className="!h-1.5 !w-1.5 !border-0 !bg-border"
      />
      <div className="flex items-center gap-1">
        {hot && (
          <Flame size={12} className="shrink-0 text-accent" aria-label={t('status.hotPath')} />
        )}
        <PlanNodeIcon kind={node.kind} />
        <span className={cn('truncate', hot ? 'font-bold' : 'font-semibold')}>{node.op}</span>
        {severity && <SeverityIcon severity={severity} size={12} />}
      </div>
      <div className="truncate text-[11px] text-muted-foreground">{objectLabel(node) || ' '}</div>
      <div className="mt-0.5 flex items-center justify-between text-[11px] tabular-nums">
        <span>{formatCount(rows)}</span>
        <span className="text-muted-foreground">
          {metricLabel}
          {share != null && ` · ${formatPercent(share)}`}
        </span>
      </div>
      {share != null && (
        <div className="mt-1 h-1 overflow-hidden rounded bg-surface-muted">
          <div className="h-full bg-accent" style={{ width: `${Math.max(2, share * 100)}%` }} />
        </div>
      )}
      {node.children.length > 0 && (
        <button
          type="button"
          aria-label={collapsed ? t('node.expand') : t('node.collapse')}
          aria-expanded={!collapsed}
          className={cn(
            'nodrag absolute z-10 rounded-full border border-border bg-surface p-0.5 hover:bg-surface-muted',
            vertical
              ? 'left-1/2 -bottom-2.5 -translate-x-1/2'
              : '-right-2.5 top-1/2 -translate-y-1/2',
          )}
          onClick={(e) => {
            e.stopPropagation();
            data.onToggle(node.id);
          }}
        >
          {vertical ? (
            collapsed ? (
              <ChevronDown size={10} />
            ) : (
              <ChevronUp size={10} />
            )
          ) : collapsed ? (
            <ChevronRight size={10} />
          ) : (
            <ChevronLeft size={10} />
          )}
        </button>
      )}
      <Handle
        type="source"
        position={vertical ? Position.Bottom : Position.Right}
        className="!h-1.5 !w-1.5 !border-0 !bg-border"
      />
    </div>
  );
});

const nodeTypes = { plan: PlanNodeCard };

function ZoomControls({
  tree,
  direction,
  onDirection,
}: {
  tree: PlanTree;
  direction: LayoutDirection;
  onDirection: (d: LayoutDirection) => void;
}) {
  const { t } = useTranslation('plan');
  const flow = useReactFlow();
  // The export always contains the whole plan, whatever is collapsed on screen.
  const save = (format: ExportFormat) =>
    void layoutPlan(tree.root, new Set())
      .then((full) => exportPlan(tree, full, format))
      .then((path) => path && toast.success(t('toolbar.exported', { path })))
      .catch((e) => toast.error(toIpcError(e).message));
  return (
    <Panel
      position="top-right"
      className="flex gap-1 rounded-md border border-border bg-surface p-0.5"
    >
      <IconButton
        label={t('toolbar.zoomIn')}
        icon={<ZoomIn size={14} />}
        onClick={() => void flow.zoomIn()}
      />
      <IconButton
        label={t('toolbar.zoomOut')}
        icon={<ZoomOut size={14} />}
        onClick={() => void flow.zoomOut()}
      />
      <IconButton
        label={t('toolbar.fit')}
        icon={<Maximize size={14} />}
        onClick={() => void flow.fitView({ padding: 0.15 })}
      />
      <IconButton
        label={direction === 'RIGHT' ? t('toolbar.layoutVertical') : t('toolbar.layoutHorizontal')}
        icon={direction === 'RIGHT' ? <Rows3 size={14} /> : <Columns3 size={14} />}
        onClick={() => onDirection(direction === 'RIGHT' ? 'DOWN' : 'RIGHT')}
      />
      <IconButton
        label={t('toolbar.exportSvg')}
        icon={<FileCode2 size={14} />}
        onClick={() => save('svg')}
      />
      <IconButton
        label={t('toolbar.exportPng')}
        icon={<ImageDown size={14} />}
        onClick={() => save('png')}
      />
    </Panel>
  );
}

/**
 * Sets the first viewport once the layout and the container size are known (and again when the
 * direction changes, signalled by `pending`): readable zoom, root top-left, instead of a fit.
 */
function InitialViewport({
  layout,
  rootId,
  pendingRef,
}: {
  layout: PlanLayout;
  rootId: number;
  pendingRef: { current: boolean };
}) {
  const flow = useReactFlow();
  const width = useStore((s) => s.width);
  const height = useStore((s) => s.height);
  useEffect(() => {
    if (!pendingRef.current || width <= 0 || height <= 0) return;
    pendingRef.current = false;
    const root = layout.positions.get(rootId) ?? { x: 0, y: 0 };
    void flow.setViewport(
      initialViewport({ width: layout.width, height: layout.height }, { width, height }, root),
    );
  }, [layout, rootId, width, height, flow, pendingRef]);
  return null;
}

/** Keeps the selected node in view, also when it was chosen in another view. */
function FollowSelection({
  selectedId,
  layout,
}: {
  selectedId: number | null;
  layout: PlanLayout;
}) {
  const flow = useReactFlow();
  useEffect(() => {
    if (selectedId == null) return;
    const p = layout.positions.get(selectedId);
    if (!p) return;
    void flow.setCenter(p.x + NODE_WIDTH / 2, p.y + NODE_HEIGHT / 2, {
      zoom: Math.max(flow.getZoom(), 0.6),
      duration: 200,
    });
    // Only when the selection changes, not on every zoom.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedId, layout]);
  return null;
}

interface PlanGraphProps {
  tree: PlanTree;
  selectedId: number | null;
  onSelect: (id: number) => void;
}

function Graph({ tree, selectedId, onSelect }: PlanGraphProps) {
  const { t } = useTranslation('plan');
  const [collapsed, setCollapsed] = useState<ReadonlySet<number>>(() => initialCollapsed(tree));
  const [layout, setLayout] = useState<PlanLayout | null>(null);
  const [layoutError, setLayoutError] = useState<string | null>(null);
  const [hover, setHover] = useState<{ id: number; x: number; y: number } | null>(null);
  const [direction, setDirection] = useGraphDirection();
  // The first layout (and the first one after a direction change) picks the viewport.
  const pendingViewport = useRef(true);
  const changeDirection = useCallback(
    (d: LayoutDirection) => {
      pendingViewport.current = true;
      setDirection(d);
    },
    [setDirection],
  );

  useEffect(() => {
    let alive = true;
    layoutPlan(tree.root, collapsed, direction)
      .then((l) => {
        if (!alive) return;
        setLayout(l);
        setLayoutError(null);
      })
      .catch((e: unknown) => alive && setLayoutError(e instanceof Error ? e.message : String(e)));
    return () => {
      alive = false;
    };
  }, [tree, collapsed, direction]);

  const toggle = useCallback(
    (id: number) =>
      setCollapsed((cur) => {
        const next = new Set(cur);
        if (!next.delete(id)) next.add(id);
        return next;
      }),
    [],
  );
  const onHover = useCallback((id: number | null, rect: DOMRect | null) => {
    // Viewport coordinates: the tooltip is positioned `fixed`.
    setHover(
      id != null && rect
        ? { id, x: Math.min(rect.right + 8, window.innerWidth - 296), y: rect.top }
        : null,
    );
  }, []);

  const { nodes, edges } = useMemo(
    () =>
      layout
        ? buildGraph(tree, layout, collapsed, { selectedId, onSelect, onToggle: toggle, onHover })
        : { nodes: [] as CardNode[], edges: [] as Edge[] },
    [layout, tree, selectedId, collapsed, onSelect, toggle, onHover],
  );

  const hovered = hover ? flatten(tree.root).find((n) => n.id === hover.id) : undefined;

  return (
    <div className="relative h-full w-full" data-testid="plan-graph" aria-label={t('views.graph')}>
      {layoutError && (
        <p
          role="alert"
          className="absolute inset-x-0 top-2 z-10 mx-auto w-fit rounded border border-danger bg-danger-soft px-3 py-1 text-xs text-danger"
        >
          {t('status.layoutFailed', { message: layoutError })}
        </p>
      )}
      <DirectionContext.Provider value={direction}>
        <ReactFlow
          nodes={nodes}
          edges={edges}
          nodeTypes={nodeTypes}
          minZoom={0.1}
          maxZoom={2}
          nodesDraggable={false}
          nodesConnectable={false}
          elementsSelectable={false}
          zoomOnDoubleClick={false}
        >
          <Background gap={20} />
          <ZoomControls tree={tree} direction={direction} onDirection={changeDirection} />
          {tree.nodeCount > MINIMAP_MIN_NODES && (
            <MiniMap pannable zoomable ariaLabel={t('toolbar.minimap')} />
          )}
          {layout && (
            <InitialViewport layout={layout} rootId={tree.root.id} pendingRef={pendingViewport} />
          )}
          {layout && <FollowSelection selectedId={selectedId} layout={layout} />}
        </ReactFlow>
      </DirectionContext.Provider>
      {hovered && hover && (
        <div
          role="tooltip"
          data-testid="plan-tooltip"
          className="pointer-events-none fixed z-50 w-72 rounded-md border border-border bg-surface p-2.5 shadow-lg"
          style={{ left: hover.x, top: hover.y }}
        >
          <NodeFacts tree={tree} node={hovered} compact />
        </div>
      )}
    </div>
  );
}

/** Operators as a diagram (xyflow + elkjs). */
export function PlanGraph(props: PlanGraphProps) {
  return (
    <ReactFlowProvider>
      <Graph {...props} />
    </ReactFlowProvider>
  );
}
