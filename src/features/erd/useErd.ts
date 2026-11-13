// State and actions of one ER diagram tab: model loading, saved layout, positions, search,
// "table + N levels" focus, saving and exporting.

import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Position, useReactFlow, type NodeHandle } from '@xyflow/react';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { toast } from '@/components/ui/toastStore';
import { ipc } from '@/lib/ipc';
import { pathKey, pathsEqual } from '@/lib/objectPath';
import type { ErdLayout, IpcError, SchemaModel, Uuid } from '@/lib/types';
import type { ErdTabPayload } from '@/stores/tabs';
import { diffLayout, makeSnapshot, type LayoutDiff } from './diff';
import { ERD_CONFLICT } from './conflict';
import { autoLayout, gridLayout } from './erdLayout';
import {
  type ExportFormat,
  type ExportTheme,
  FORMAT_FILTERS,
  bytesToBase64,
  renderExport,
} from './exportImage';
import { anchorOffset, boundsOf, edgeGeometry, nodeSize, type Rect } from './geometry';
import {
  DEFAULT_OPTIONS,
  type ErdView,
  type LayoutOptions,
  type Point,
  buildView,
  parseLayoutTables,
  parseOptions,
  parseSnapshot,
  parseViewport,
  relationKey,
  tableKey,
  tableLabel,
  type LayoutTable,
  type Snapshot,
  type Viewport,
} from './model';
import { relationEnds } from './notation';
import { matchTables, neighborhood } from './selection';
import type { RelationFlowEdge } from './CrowsFootEdge';
import type { TableFlowNode } from './TableNode';

const EMPTY_VIEW: ErdView = { tables: [], byKey: new Map(), relations: [] };

export const erdKeys = {
  model: (sessionId: Uuid, scope: ErdTabPayload['scope'], showViews: boolean) =>
    ['erd', 'model', sessionId, pathKey(scope), showViews] as const,
  layouts: (connectionId: Uuid) => ['erd', 'layouts', connectionId] as const,
  layout: (id: Uuid) => ['erd', 'layout', id] as const,
  ddl: (sessionId: Uuid, path: ErdTabPayload['scope']) =>
    ['erd', 'ddl', sessionId, pathKey(path)] as const,
};

export interface Focus {
  key: string;
  depth: number;
}

const GHOST_SIZE = { width: 180, height: 64 };

/**
 * Handles declared up front (an edge needs them, and the geometry is known): edges render on
 * the first pass without waiting for the DOM to be measured.
 */
function handlesFor(width: number, height: number): NodeHandle[] {
  return [
    { type: 'target', position: Position.Left, x: 0, y: height / 2, width: 1, height: 1 },
    { type: 'source', position: Position.Right, x: width, y: height / 2, width: 1, height: 1 },
  ];
}

function errorMessage(e: unknown): string {
  return (e as Partial<IpcError> | undefined)?.message ?? String(e);
}

export function useErd({ sessionId, connectionId, scope, layoutId }: ErdTabPayload) {
  const queryClient = useQueryClient();
  const flow = useReactFlow();

  const [activeLayoutId, setActiveLayoutId] = useState<Uuid | null>(layoutId);
  const [layoutName, setLayoutName] = useState<string | null>(null);
  const layoutQuery = useQuery({
    queryKey: erdKeys.layout(activeLayoutId ?? 'none'),
    queryFn: () => ipc.erdLayoutGet(activeLayoutId as Uuid),
    enabled: activeLayoutId !== null,
    retry: false,
    staleTime: Infinity,
  });
  const layout: ErdLayout | undefined = layoutQuery.data;

  const [options, setOptions] = useState<LayoutOptions>(DEFAULT_OPTIONS);
  const [positions, setPositions] = useState<Record<string, Point>>({});
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(new Set());
  const [colors, setColors] = useState<ReadonlyMap<string, string>>(new Map());
  const [viewport, setViewport] = useState<Viewport | null>(null);
  const [seededFor, setSeededFor] = useState<string | null>(null);
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [focus, setFocus] = useState<Focus | null>(null);
  const [search, setSearch] = useState('');
  const [showDdl, setShowDdl] = useState(false);
  const [layingOut, setLayingOut] = useState(false);
  // What the stored layout holds, kept apart from the tables on screen so that saving never
  // drops a table that is merely not shown (hidden views, tables missing from the database).
  const [savedTables, setSavedTables] = useState<LayoutTable[]>([]);
  const [savedSnapshot, setSavedSnapshot] = useState<Snapshot>({});
  const [dropped, setDropped] = useState<ReadonlySet<string>>(new Set());
  const [conflict, setConflict] = useState<{ name: string; asNew: boolean } | null>(null);

  // 1. Seed the state from the saved layout (or the defaults) once it is known. This adjusts
  // state during render (React's pattern for state derived from a changing input) so the first
  // paint already has the seeded positions.
  const seedId = activeLayoutId ?? 'none';
  const layoutLoaded = activeLayoutId === null || layoutQuery.isSuccess || layoutQuery.isError;
  if (layoutLoaded && seededFor !== seedId) {
    setSeededFor(seedId);
    if (layout && layout.id === activeLayoutId) {
      const saved = parseLayoutTables(layout.tables);
      setOptions(parseOptions(layout.options));
      setPositions(Object.fromEntries(saved.map((t) => [tableKey(t.path), { x: t.x, y: t.y }])));
      setCollapsed(new Set(saved.filter((t) => t.collapsed).map((t) => tableKey(t.path))));
      setColors(
        new Map(saved.flatMap((t) => (t.color ? [[tableKey(t.path), t.color] as const] : []))),
      );
      setViewport(parseViewport(layout.viewport));
      setLayoutName(layout.name);
      setSavedTables(saved);
      setSavedSnapshot(parseSnapshot(layout.snapshot));
      setDropped(new Set());
    } else {
      setPositions({});
      setViewport(null);
      setSavedTables([]);
      setSavedSnapshot({});
      setDropped(new Set());
    }
  }
  const seeded = seededFor === seedId;

  // 2. The model.
  const modelQuery = useQuery<SchemaModel, IpcError>({
    queryKey: erdKeys.model(sessionId, scope, options.showViews),
    queryFn: () =>
      ipc.schemaModel(sessionId, {
        scope,
        includeViews: options.showViews,
        includeComments: true,
        detail: 'full',
      }),
    enabled: seeded,
    retry: false,
    staleTime: 60_000,
  });
  const model = modelQuery.data;
  const view = useMemo(() => (model ? buildView(model, options) : EMPTY_VIEW), [model, options]);

  // 3. Place the tables without a saved position.
  const positionsRef = useRef(positions);
  useEffect(() => {
    positionsRef.current = positions;
  }, [positions]);
  useEffect(() => {
    if (!seeded || !model) return;
    const missing = view.tables.filter((t) => !positionsRef.current[t.key]);
    if (missing.length === 0) return;
    let cancelled = false;
    const existing = Object.keys(positionsRef.current).length > 0;
    void (async () => {
      setLayingOut(true);
      try {
        let placed: Map<string, Point>;
        try {
          placed = await autoLayout({
            tables: missing,
            relations: view.relations,
            options,
            collapsed,
          });
        } catch {
          placed = gridLayout(missing, options);
        }
        if (cancelled) return;
        // New tables of an existing layout go below what is already there.
        let dy = 0;
        if (existing) {
          const rects = view.tables
            .filter((t) => positionsRef.current[t.key])
            .map((t) => ({
              ...positionsRef.current[t.key]!,
              ...nodeSize(t, collapsed.has(t.key), options),
            }));
          const b = boundsOf(rects);
          dy = b ? b.y + b.height + 80 : 0;
        }
        setPositions((prev) => {
          const next = { ...prev };
          for (const [k, p] of placed) if (!next[k]) next[k] = { x: p.x, y: p.y + dy };
          return next;
        });
      } finally {
        setLayingOut(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [seeded, model, view, options, collapsed]);

  const ready =
    seeded && model !== undefined && view.tables.every((t) => positions[t.key] !== undefined);

  // 4. Differences with the saved layout.
  const diff: LayoutDiff = useMemo(() => {
    if (!model || savedTables.length === 0) return { missing: [], added: [], changed: [] };
    // Views are not part of the model while they are hidden: they are not "missing" then.
    const relevant = savedTables.filter(
      (t) =>
        !dropped.has(tableKey(t.path)) &&
        (options.showViews || t.path.segments[t.path.segments.length - 1]?.kind === 'table'),
    );
    return diffLayout(relevant, savedSnapshot, view.tables);
  }, [model, savedTables, savedSnapshot, dropped, options.showViews, view]);

  // 5. Focus, search and selection.
  const visibleKeys = useMemo(
    () => (focus ? neighborhood(view.relations, focus.key, focus.depth) : null),
    [focus, view],
  );
  const visibleTables = useMemo(
    () => (visibleKeys ? view.tables.filter((t) => visibleKeys.has(t.key)) : view.tables),
    [view, visibleKeys],
  );
  const matches = useMemo(
    () =>
      new Set(
        matchTables(
          view.tables.map((t) => ({ key: t.key, label: tableLabel(t.table) })),
          search,
        ).map((m) => m.key),
      ),
    [view, search],
  );

  const sizes = useMemo(() => {
    const m = new Map<string, { width: number; height: number }>();
    for (const t of view.tables) m.set(t.key, nodeSize(t, collapsed.has(t.key), options));
    return m;
  }, [view, collapsed, options]);

  const toggleCollapsed = useCallback((key: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (!next.delete(key)) next.add(key);
      return next;
    });
  }, []);

  const nodes: TableFlowNode[] = useMemo(() => {
    if (!ready) return [];
    const out: TableFlowNode[] = visibleTables.map((t) => {
      const size = sizes.get(t.key)!;
      const status = diff.added.includes(t.key)
        ? 'new'
        : diff.changed.includes(t.key)
          ? 'changed'
          : 'ok';
      return {
        id: t.key,
        type: 'table',
        position: positions[t.key]!,
        width: size.width,
        height: size.height,
        handles: handlesFor(size.width, size.height),
        selected: t.key === selectedKey,
        data: {
          table: t,
          collapsed: collapsed.has(t.key),
          color: colors.get(t.key),
          showTypes: options.showTypes,
          dim: search.trim() !== '' && !matches.has(t.key),
          match: search.trim() !== '' && matches.has(t.key),
          status,
          width: size.width,
          height: size.height,
          onToggle: toggleCollapsed,
        },
      };
    });
    if (!focus) {
      for (const g of diff.missing) {
        const key = tableKey(g.path);
        const name = g.path.segments[g.path.segments.length - 1]?.name ?? key;
        out.push({
          id: key,
          type: 'table',
          position: { x: g.x, y: g.y },
          ...GHOST_SIZE,
          handles: handlesFor(GHOST_SIZE.width, GHOST_SIZE.height),
          data: {
            ghostLabel: name,
            collapsed: true,
            showTypes: false,
            dim: false,
            match: false,
            status: 'missing',
            ...GHOST_SIZE,
            onToggle: toggleCollapsed,
          },
        });
      }
    }
    return out;
  }, [
    ready,
    visibleTables,
    sizes,
    positions,
    selectedKey,
    collapsed,
    colors,
    options.showTypes,
    search,
    matches,
    diff,
    focus,
    toggleCollapsed,
  ]);

  const edges: RelationFlowEdge[] = useMemo(() => {
    if (!ready) return [];
    const shown = new Set(visibleTables.map((t) => t.key));
    const out: RelationFlowEdge[] = [];
    for (const r of view.relations) {
      const a = tableKey(r.fromTable);
      const b = tableKey(r.toTable);
      if (!shown.has(a) || !shown.has(b)) continue;
      const from = view.byKey.get(a)!;
      const to = view.byKey.get(b)!;
      const sa = sizes.get(a)!;
      const sb = sizes.get(b)!;
      const pa = positions[a]!;
      const pb = positions[b]!;
      const ra: Rect = { ...pa, ...sa };
      const rb: Rect = { ...pb, ...sb };
      const geometry = edgeGeometry(
        ra,
        rb,
        pa.y + anchorOffset(from, r.fromColumns, collapsed.has(a), sa.height),
        pb.y + anchorOffset(to, r.toColumns, collapsed.has(b), sb.height),
      );
      const ends = relationEnds(r);
      const touchesSelection = selectedKey !== null && (a === selectedKey || b === selectedKey);
      out.push({
        id: relationKey(r),
        source: a,
        target: b,
        type: 'relation',
        data: {
          geometry,
          child: ends.child,
          parent: ends.parent,
          highlight: touchesSelection,
          dim: selectedKey !== null && !touchesSelection,
          label: `${r.name}: ${r.fromColumns.join(', ')} → ${r.toColumns.join(', ')}`,
        },
      });
    }
    return out;
  }, [ready, visibleTables, view, sizes, positions, collapsed, selectedKey]);

  const fit = useCallback(() => {
    requestAnimationFrame(() => void flow.fitView({ padding: 0.15, duration: 250 }));
  }, [flow]);

  const move = useCallback((key: string, position: Point) => {
    setPositions((prev) => ({ ...prev, [key]: position }));
  }, []);

  const relayout = useCallback(async () => {
    setLayingOut(true);
    try {
      let placed: Map<string, Point>;
      try {
        placed = await autoLayout({
          tables: visibleTables,
          relations: view.relations,
          options,
          collapsed,
        });
      } catch {
        placed = gridLayout(visibleTables, options);
      }
      setPositions((prev) => ({ ...prev, ...Object.fromEntries(placed) }));
      fit();
    } finally {
      setLayingOut(false);
    }
  }, [visibleTables, view, options, collapsed, fit]);

  const focusOnTable = useCallback(
    (key: string) => {
      const p = positions[key];
      const size = sizes.get(key);
      if (!p || !size) return;
      void flow.setCenter(p.x + size.width / 2, p.y + size.height / 2, { zoom: 1, duration: 300 });
      setSelectedKey(key);
    },
    [flow, positions, sizes],
  );

  const refresh = useCallback(() => {
    void queryClient.invalidateQueries({ queryKey: ['erd', 'model', sessionId] });
    void modelQuery.refetch();
  }, [queryClient, sessionId, modelQuery]);

  const save = useCallback(
    async (name: string, asNew: boolean, baseUpdatedAt?: string) => {
      if (!model) return null;
      const existing = !asNew && activeLayoutId !== null ? layout : undefined;
      const inView = new Set(view.tables.map((t) => t.key));
      const modelKeys = new Set(model.tables.map((t) => tableKey(t.path)));
      // Tables that are stored but not on screen keep their place (until the user forgets them).
      const kept = savedTables.filter((t) => {
        const k = tableKey(t.path);
        return !inView.has(k) && !dropped.has(k);
      });
      const body: ErdLayout = {
        id: existing?.id ?? crypto.randomUUID(),
        connectionId,
        name,
        scope,
        tables: [
          ...view.tables.map((t) => ({
            path: t.table.path,
            x: positions[t.key]?.x ?? 0,
            y: positions[t.key]?.y ?? 0,
            collapsed: collapsed.has(t.key),
            ...(colors.get(t.key) ? { color: colors.get(t.key) } : {}),
          })),
          ...kept.map((t) => {
            const k = tableKey(t.path);
            return {
              path: t.path,
              x: positions[k]?.x ?? t.x,
              y: positions[k]?.y ?? t.y,
              collapsed: t.collapsed,
              ...(t.color ? { color: t.color } : {}),
            };
          }),
        ],
        options,
        viewport: flow.getViewport(),
        snapshot: {
          ...Object.fromEntries(
            Object.entries(savedSnapshot).filter(([k]) => !modelKeys.has(k) && !dropped.has(k)),
          ),
          ...makeSnapshot(model.tables),
        },
        createdAt: existing?.createdAt ?? '',
        updatedAt: baseUpdatedAt ?? existing?.updatedAt ?? '',
      };
      try {
        const saved = await ipc.erdLayoutSave(body);
        queryClient.setQueryData(erdKeys.layout(saved.id), saved);
        void queryClient.invalidateQueries({ queryKey: erdKeys.layouts(connectionId) });
        setSeededFor(saved.id);
        setActiveLayoutId(saved.id);
        setLayoutName(saved.name);
        setSavedTables(parseLayoutTables(saved.tables));
        setSavedSnapshot(parseSnapshot(saved.snapshot));
        setDropped(new Set());
        return saved;
      } catch (e) {
        if (errorMessage(e).startsWith(ERD_CONFLICT)) {
          setConflict({ name, asNew });
        } else {
          toast.error(errorMessage(e));
        }
        return null;
      }
    },
    [
      model,
      activeLayoutId,
      layout,
      connectionId,
      scope,
      view,
      positions,
      collapsed,
      colors,
      options,
      flow,
      queryClient,
      savedTables,
      savedSnapshot,
      dropped,
    ],
  );

  /** The stored layout changed elsewhere: overwrite it, or load the stored version. */
  const resolveConflict = useCallback(
    async (choice: 'overwrite' | 'reload') => {
      const pending = conflict;
      setConflict(null);
      if (!pending || activeLayoutId === null) return;
      if (choice === 'overwrite') {
        try {
          const latest = await ipc.erdLayoutGet(activeLayoutId);
          await save(pending.name, pending.asNew, latest.updatedAt);
        } catch (e) {
          toast.error(errorMessage(e));
        }
      } else {
        await layoutQuery.refetch();
        setSeededFor(null);
      }
    },
    [conflict, activeLayoutId, save, layoutQuery],
  );

  const openLayout = useCallback((id: Uuid) => {
    setSelectedKey(null);
    setFocus(null);
    setSeededFor(null);
    setActiveLayoutId(id);
  }, []);

  const deleteLayout = useCallback(
    async (id: Uuid) => {
      try {
        await ipc.erdLayoutDelete(id);
        queryClient.removeQueries({ queryKey: erdKeys.layout(id) });
        void queryClient.invalidateQueries({ queryKey: erdKeys.layouts(connectionId) });
        if (id === activeLayoutId) {
          // Keep what is on screen: the diagram simply becomes an unsaved one.
          setSeededFor('none');
          setSavedTables([]);
          setSavedSnapshot({});
          setActiveLayoutId(null);
          setLayoutName(null);
        }
      } catch (e) {
        toast.error(errorMessage(e));
      }
    },
    [queryClient, connectionId, activeLayoutId],
  );

  const exportAs = useCallback(
    async (format: ExportFormat, theme: ExportTheme | 'current'): Promise<string | null> => {
      const resolved: ExportTheme =
        theme === 'current'
          ? document.documentElement.classList.contains('dark')
            ? 'dark'
            : 'light'
          : theme;
      const shown = new Set(visibleTables.map((t) => t.key));
      const bytes = await renderExport(
        {
          tables: visibleTables,
          relations: view.relations.filter(
            (r) => shown.has(tableKey(r.fromTable)) && shown.has(tableKey(r.toTable)),
          ),
          positions: new Map(Object.entries(positions)),
          collapsed,
          colors,
          options,
          theme: resolved,
        },
        format,
      );
      const base = (scope.segments[scope.segments.length - 1]?.name ?? 'diagram').replace(
        /[^\p{L}\p{N}._-]+/gu,
        '_',
      );
      return ipc.fileSaveBytes(`${base}.${format}`, bytesToBase64(bytes), [FORMAT_FILTERS[format]]);
    },
    [visibleTables, view, positions, collapsed, colors, options, scope],
  );

  const selectedTable = selectedKey ? view.byKey.get(selectedKey) : undefined;

  return {
    // data
    model,
    modelError: modelQuery.error,
    modelLoading: modelQuery.isPending || !seeded,
    view,
    nodes,
    edges,
    ready,
    layingOut,
    truncated: model?.truncated ?? false,
    conflict,
    resolveConflict,
    forgetMissing: () => setDropped(new Set(diff.missing.map((m) => tableKey(m.path)))),
    viewport,
    diff,
    // state
    options,
    setOptions,
    search,
    setSearch,
    matches,
    selectedKey,
    setSelectedKey,
    selectedTable,
    focus,
    setFocus,
    visibleCount: visibleTables.length,
    showDdl,
    setShowDdl,
    activeLayoutId,
    layoutName,
    samePathScope: (p: ErdTabPayload['scope']) => pathsEqual(p, scope),
    // actions
    move,
    relayout,
    fit,
    focusOnTable,
    refresh,
    save,
    openLayout,
    deleteLayout,
    exportAs,
    setColor: (key: string, color: string | null) =>
      setColors((prev) => {
        const next = new Map(prev);
        if (color) next.set(key, color);
        else next.delete(key);
        return next;
      }),
  };
}
