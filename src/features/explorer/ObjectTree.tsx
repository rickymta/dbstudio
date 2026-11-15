import { useAppearanceStore } from '@/stores/appearance';
import { useQueryClient } from '@tanstack/react-query';
import { useVirtualizer } from '@tanstack/react-virtual';
import { ChevronRight, RefreshCw, X } from 'lucide-react';
import {
  memo,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
  type ReactNode,
} from 'react';
import { useTranslation } from 'react-i18next';
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuSub,
  ContextMenuSubContent,
  ContextMenuSubTrigger,
  ContextMenuTrigger,
} from '@/components/ui/ContextMenu';
import { Badge } from '@/components/ui/Badge';
import { Spinner } from '@/components/ui/Spinner';
import { toast } from '@/components/ui/toastStore';
import { DeleteByPatternDialog } from '@/features/keyvalue/DeleteByPatternDialog';
import { parseKeyDetail, formatDuration } from '@/features/keyvalue/keyDetail';
import { cn } from '@/lib/cn';
import { pathKey, ROOT_PATH } from '@/lib/objectPath';
import type {
  Capabilities,
  DriverDescriptor,
  ObjectNode,
  ObjectPath,
  ScriptTemplate,
  SessionInfo,
  Uuid,
} from '@/lib/types';
import { useSession } from '@/stores/sessions';
import { ddlTabId, tableTabId, useTabsStore } from '@/stores/tabs';
import { explorerActions, scriptTemplates, type ExplorerAction } from './actions';
import { copyScript, openScriptInEditor } from './scriptAs';
import { KindIcon } from './kindIcons';
import { PatternFilterDialog } from './PatternFilterDialog';
import { openObjectTab } from './openObject';
import { revealTuning, useRevealStore } from './revealStore';
import {
  clearBranch,
  clearSessionCache,
  getLevelPattern,
  loadChildren,
  loadMoreChildren,
  readChildrenState,
  readPaging,
  setLevelPattern,
  useTreeCacheVersion,
} from './queries';
import {
  flattenTree,
  isPagingRow,
  navigate,
  nodeLabel,
  nodeActions,
  qualifiedName,
  type NavKey,
  type NodeAction,
  type NodeRow,
  type PagingRow,
  type StatusRow,
  type TreeRow,
} from './treeModel';

export interface ObjectTreeProps {
  connectionId: string;
  /** Case-insensitive name filter applied to the already-loaded nodes (BROWSE-06). */
  filter?: string;
  /** Rendered inside another tree (the connections sidebar): use `group` instead of `tree`. */
  nested?: boolean;
}

const ROW_HEIGHT = 24;
const INDENT = 10;
const NAV_KEYS: ReadonlySet<string> = new Set([
  'ArrowUp',
  'ArrowDown',
  'ArrowLeft',
  'ArrowRight',
  'Home',
  'End',
]);

/** Lazy, virtualized object tree of an open connection. Renders nothing without a session. */
export function ObjectTree({ connectionId, filter, nested }: ObjectTreeProps) {
  const session = useSession(connectionId);
  if (!session) return null;
  return <SessionTree key={session.sessionId} session={session} filter={filter} nested={nested} />;
}

function SessionTree({
  session,
  filter,
  nested,
}: {
  session: SessionInfo;
  filter?: string;
  nested?: boolean;
}) {
  const { sessionId, connectionId } = session;
  const appearance = useAppearanceStore((s) => s.appearance);
  const ROW_HEIGHT = Math.max(
    appearance.density === 'compact' ? 26 : 30,
    appearance.uiFontSize + 12,
  );
  const capabilities = session.descriptor.capabilities;
  const { t } = useTranslation('explorer');
  const qc = useQueryClient();
  const openTab = useTabsStore((s) => s.openTab);
  const version = useTreeCacheVersion(sessionId);
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(() => new Set());
  const [focusKey, setFocusKey] = useState<string | null>(null);
  const scrollRef = useRef<HTMLDivElement>(null);

  const label = useCallback((node: ObjectNode) => nodeLabel(t, node), [t]);

  const load = useCallback(
    (path: ObjectPath) => {
      loadChildren(qc, sessionId, path, capabilities.pagedTree).catch(() => {
        // The error is kept in the query cache and rendered on the row.
      });
    },
    [qc, sessionId, capabilities.pagedTree],
  );

  const [dialog, setDialog] = useState<{ type: 'filter' | 'delete'; node: ObjectNode } | null>(
    null,
  );

  useEffect(() => {
    load(ROOT_PATH);
    return () => clearSessionCache(qc, sessionId);
  }, [load, qc, sessionId]);

  // `version` changes whenever the cache does, which is what invalidates this memo.
  const rows = useMemo(
    () =>
      flattenTree({
        getChildren: (p) => readChildrenState(qc, sessionId, p),
        getPaging: capabilities.pagedTree ? (p) => readPaging(qc, sessionId, p) : undefined,
        expanded,
        filter,
        label,
      }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [qc, sessionId, expanded, filter, label, version, capabilities.pagedTree],
  );

  // Reveal requests ("show in tree"): expand every ancestor, then focus the target row.
  const reveal = useRevealStore((s) => s.request);
  const [pendingFocus, setPendingFocus] = useState<string | null>(null);
  const revealToken = useRef(0);
  useEffect(() => {
    if (!reveal || reveal.connectionId !== connectionId) return;
    useRevealStore.getState().consume(reveal.nonce);
    setPendingFocus(null); // a newer request replaces an unfinished one
    const token = ++revealToken.current;
    void (async () => {
      const segments = reveal.path.segments;
      for (let i = 1; i < segments.length; i++) {
        const ancestor = { segments: segments.slice(0, i) };
        setExpanded((prev) => new Set(prev).add(pathKey(ancestor)));
        try {
          await loadChildren(qc, sessionId, ancestor, capabilities.pagedTree);
          if (token !== revealToken.current) return;
        } catch {
          if (token === revealToken.current) setPendingFocus(null);
          return; // the error row shows where loading failed
        }
      }
      if (token === revealToken.current) setPendingFocus(pathKey(reveal.path));
    })();
  }, [reveal, connectionId, qc, sessionId, capabilities.pagedTree]);

  // eslint-disable-next-line react-hooks/incompatible-library
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 12,
    getItemKey: (i) => rows[i]!.key,
  });

  useEffect(() => {
    virtualizer.measure();
  }, [ROW_HEIGHT, virtualizer]);

  // Give up when the target row never shows up, so focus cannot jump much later.
  useEffect(() => {
    if (!pendingFocus) return;
    const id = setTimeout(() => setPendingFocus(null), revealTuning.focusTimeoutMs);
    return () => clearTimeout(id);
  }, [pendingFocus]);

  useEffect(() => {
    if (!pendingFocus) return;
    const index = rows.findIndex((r) => r.key === pendingFocus);
    if (index < 0) return;
    setFocusKey(pendingFocus);
    virtualizer.scrollToIndex(index, { align: 'center' });
    scrollRef.current?.focus();
    setPendingFocus(null);
  }, [pendingFocus, rows, virtualizer]);

  const setNodeExpanded = useCallback(
    (row: NodeRow, expand: boolean) => {
      if (!row.node.hasChildren) return;
      setExpanded((prev) => {
        const next = new Set(prev);
        if (expand) next.add(row.key);
        else next.delete(row.key);
        return next;
      });
      if (expand) load(row.node.path);
    },
    [load],
  );

  const refresh = useCallback(
    (row: NodeRow) => {
      clearBranch(qc, sessionId, row.node.path);
      // Forget expanded descendants, they will be collapsed after the reload.
      setExpanded((prev) => {
        const next = new Set(prev);
        for (const key of prev)
          if (key !== row.key && key.startsWith(row.key.slice(0, -1) + ',')) next.delete(key);
        return next;
      });
      if (row.node.hasChildren && expanded.has(row.key)) load(row.node.path);
    },
    [qc, sessionId, expanded, load],
  );

  const activate = useCallback(
    (row: NodeRow) => {
      if (!openObjectTab(row.node, session) && row.node.hasChildren) {
        setNodeExpanded(row, !row.expanded);
      }
    },
    [session, setNodeExpanded],
  );

  const runAction = useCallback(
    (action: NodeAction, row: NodeRow) => {
      const payload = { sessionId, connectionId, path: row.node.path };
      switch (action) {
        case 'openData':
          openTab({
            id: tableTabId(sessionId, row.node.path),
            kind: 'table',
            title: row.node.name,
            closable: true,
            payload,
          });
          break;
        case 'openKey':
          openObjectTab(row.node, session);
          break;
        case 'filterPattern':
          setDialog({ type: 'filter', node: row.node });
          break;
        case 'deleteByPattern':
          setDialog({ type: 'delete', node: row.node });
          break;
        case 'viewDdl':
          openTab({
            id: ddlTabId(sessionId, row.node.path),
            kind: 'ddl',
            title: row.node.name,
            closable: true,
            payload,
          });
          break;
        case 'copyName':
          copyText(row.node.name, t('copied'));
          break;
        case 'copyFullName':
          copyText(qualifiedName(row.node.path), t('copied'));
          break;
        case 'refresh':
          refresh(row);
          break;
      }
    },
    [sessionId, connectionId, session, openTab, refresh, t],
  );

  const applyPattern = useCallback(
    (node: ObjectNode, pattern: string) => {
      setLevelPattern(qc, sessionId, node.path, pattern);
      setExpanded((prev) => new Set(prev).add(pathKey(node.path)));
      load(node.path);
    },
    [qc, sessionId, load],
  );

  const runScript = useCallback(
    (node: ObjectNode, template: ScriptTemplate, copy: boolean) =>
      void (copy ? copyScript : openScriptInEditor)(session, node, template),
    [session],
  );
  const runExplorerAction = useCallback((action: ExplorerAction) => action.run(session), [session]);
  const loadMore = useCallback(
    (path: ObjectPath) => void loadMoreChildren(qc, sessionId, path),
    [qc, sessionId],
  );
  const clearPattern = useCallback(
    (path: ObjectPath) => {
      setLevelPattern(qc, sessionId, path, '');
      load(path);
    },
    [qc, sessionId, load],
  );

  const indexOfKey = useCallback((key: string) => rows.findIndex((r) => r.key === key), [rows]);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const current = rows.find((r): r is NodeRow => r.type === 'node' && r.key === focusKey);
    if (e.key === 'Enter' || (e.key === ' ' && current)) {
      if (current) {
        e.preventDefault();
        activate(current);
      }
      return;
    }
    if (!NAV_KEYS.has(e.key)) return;
    e.preventDefault();
    const result = navigate(rows, focusKey, e.key as NavKey);
    if (result.toggle) {
      const row = rows.find((r) => r.key === result.toggle!.key);
      if (row?.type === 'node') setNodeExpanded(row, result.toggle.expand);
    }
    if (result.focusKey && result.focusKey !== focusKey) {
      setFocusKey(result.focusKey);
      const index = indexOfKey(result.focusKey);
      if (index >= 0) virtualizer.scrollToIndex(index);
    }
  };

  const onFocus = () => {
    const first = rows.find((r) => r.type === 'node');
    if (first) setFocusKey((current) => current ?? first.key);
  };

  // aria-activedescendant must name an element that exists; virtualized rows may be unmounted.
  const virtualItems = virtualizer.getVirtualItems();
  const activeRendered =
    focusKey !== null && virtualItems.some((item) => rows[item.index]?.key === focusKey);
  const activeId = activeRendered && focusKey ? itemId(sessionId, focusKey) : undefined;

  return (
    <div
      ref={scrollRef}
      role={nested ? 'group' : 'tree'}
      tabIndex={0}
      aria-label={t('treeLabel')}
      aria-activedescendant={activeId}
      onKeyDown={onKeyDown}
      onFocus={onFocus}
      data-testid="object-tree"
      className="object-tree group/tree max-h-[60vh] overflow-auto outline-none focus-visible:outline-none"
    >
      {rows.length === 0 && <TreeSkeleton sessionId={sessionId} />}
      <div style={{ height: virtualizer.getTotalSize(), position: 'relative', minWidth: '100%' }}>
        {virtualItems.map((item) => {
          const row = rows[item.index];
          if (!row) return null;
          return (
            <div
              key={row.key}
              style={{
                position: 'absolute',
                top: 0,
                left: 0,
                width: '100%',
                height: ROW_HEIGHT,
                transform: `translateY(${item.start}px)`,
              }}
            >
              {row.type === 'node' ? (
                <TreeNodeRow
                  row={row}
                  sessionId={sessionId}
                  capabilities={capabilities}
                  descriptor={session.descriptor}
                  label={label(row.node)}
                  selected={row.key === focusKey}
                  onSelect={setFocusKey}
                  onToggle={setNodeExpanded}
                  onActivate={activate}
                  onAction={runAction}
                  onExplorerAction={runExplorerAction}
                  onScript={runScript}
                />
              ) : isPagingRow(row) ? (
                <PagingRowView row={row} onLoadMore={loadMore} onClearPattern={clearPattern} />
              ) : (
                <StatusRowView row={row} onRetry={load} />
              )}
            </div>
          );
        })}
      </div>
      {dialog?.type === 'filter' && (
        <PatternFilterDialog
          open
          title={nodeLabel(t, dialog.node)}
          initial={getLevelPattern(sessionId, dialog.node.path)}
          onOpenChange={(open) => !open && setDialog(null)}
          onApply={(pattern) => applyPattern(dialog.node, pattern)}
        />
      )}
      {dialog?.type === 'delete' && (
        <DeleteByPatternDialog
          open
          sessionId={sessionId}
          node={dialog.node}
          onOpenChange={(open) => !open && setDialog(null)}
          onDone={() => refresh(nodeRowFor(dialog.node))}
        />
      )}
    </div>
  );
}

/** Minimal row for `refresh`, which only needs the node and its key. */
function nodeRowFor(node: ObjectNode): NodeRow {
  return {
    type: 'node',
    key: pathKey(node.path),
    node,
    depth: 0,
    expanded: false,
    loading: false,
    parentKey: null,
  };
}

const itemId = (sessionId: Uuid, key: string) => `tree-${sessionId}-${key}`;

function copyText(text: string, message: string) {
  void navigator.clipboard
    ?.writeText(text)
    .then(() => toast.info(message))
    .catch(() => undefined);
}

function TreeSkeleton({ sessionId }: { sessionId: Uuid }) {
  const qc = useQueryClient();
  const state = readChildrenState(qc, sessionId, ROOT_PATH);
  if (state.status !== 'loading' && state.status !== 'idle') return null;
  return (
    <div className="flex items-center gap-2 px-3" style={{ height: ROW_HEIGHT }}>
      <Spinner size={12} />
    </div>
  );
}

interface NodeRowProps {
  row: NodeRow;
  sessionId: Uuid;
  capabilities: Capabilities;
  descriptor: DriverDescriptor;
  label: string;
  selected: boolean;
  onSelect: (key: string) => void;
  onToggle: (row: NodeRow, expand: boolean) => void;
  onActivate: (row: NodeRow) => void;
  onAction: (action: NodeAction, row: NodeRow) => void;
  onExplorerAction: (action: ExplorerAction) => void;
  onScript: (node: ObjectNode, template: ScriptTemplate, copy: boolean) => void;
}

const TreeNodeRow = memo(function TreeNodeRow({
  row,
  sessionId,
  capabilities,
  descriptor,
  label,
  selected,
  onSelect,
  onToggle,
  onActivate,
  onAction,
  onExplorerAction,
  onScript,
}: NodeRowProps) {
  const { t } = useTranslation('explorer');
  const { node } = row;
  const actions = nodeActions(node, capabilities);
  const extraActions = explorerActions(node, descriptor);
  const templates = scriptTemplates(node, descriptor);
  const keyDetail = node.kind === 'key' ? parseKeyDetail(node.detail) : null;
  const content: ReactNode = (
    <div
      id={itemId(sessionId, row.key)}
      role="treeitem"
      aria-level={row.depth + 1}
      aria-selected={selected}
      aria-expanded={node.hasChildren ? row.expanded : undefined}
      title={node.detail ? `${label} - ${node.detail}` : label}
      onClick={() => onSelect(row.key)}
      onDoubleClick={() => onActivate(row)}
      style={{ paddingLeft: 4 + Math.min(row.depth, 6) * INDENT, height: '100%' }}
      className={cn(
        'flex cursor-default items-center gap-1 pr-2 text-[13px] hover:bg-surface-muted',
        selected && 'bg-surface-muted group-focus-within/tree:bg-accent/20',
      )}
    >
      <span className="flex size-4 shrink-0 items-center justify-center">
        {row.loading ? (
          <Spinner size={12} />
        ) : node.hasChildren ? (
          <button
            type="button"
            tabIndex={-1}
            aria-label={row.expanded ? t('collapse') : t('expand')}
            onClick={(e) => {
              e.stopPropagation();
              onSelect(row.key);
              onToggle(row, !row.expanded);
            }}
            onDoubleClick={(e) => e.stopPropagation()}
            className="rounded text-muted-foreground hover:text-foreground"
          >
            <ChevronRight
              size={12}
              className={cn('transition-transform', row.expanded && 'rotate-90')}
            />
          </button>
        ) : null}
      </span>
      <KindIcon kind={node.kind} className="shrink-0 text-muted-foreground" />
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {keyDetail ? (
        <span className="ml-auto flex shrink-0 items-center gap-1 pl-2">
          {keyDetail.ttlSeconds !== null && (
            <span className="text-[11px] text-muted-foreground">
              {formatDuration(keyDetail.ttlSeconds)}
            </span>
          )}
          <Badge data-testid="key-type-badge">{keyDetail.type}</Badge>
        </span>
      ) : (
        node.detail && (
          <span className="object-detail ml-auto shrink-0 truncate pl-2 text-xs text-muted-foreground">
            {node.detail}
          </span>
        )
      )}
    </div>
  );
  if (actions.length === 0 && extraActions.length === 0 && templates.length === 0) return content;
  return (
    <ContextMenu onOpenChange={(open) => open && onSelect(row.key)}>
      <ContextMenuTrigger asChild>{content}</ContextMenuTrigger>
      <ContextMenuContent>
        {actions.map((a) => (
          <ContextMenuItem key={a} onSelect={() => onAction(a, row)}>
            {a === 'refresh' && <RefreshCw size={12} />}
            {t(`action.${a}`)}
          </ContextMenuItem>
        ))}
        {extraActions.map((a) => (
          <ContextMenuItem key={a.id} onSelect={() => onExplorerAction(a)}>
            {t(a.labelKey)}
          </ContextMenuItem>
        ))}
        {templates.length > 0 && (
          <>
            <ContextMenuSeparator />
            {([false, true] as const).map((copy) => (
              <ContextMenuSub key={String(copy)}>
                <ContextMenuSubTrigger>
                  {t(copy ? 'script.copyAs' : 'script.scriptAs')}
                </ContextMenuSubTrigger>
                <ContextMenuSubContent>
                  {templates.map((tpl) => (
                    <ContextMenuItem key={tpl} onSelect={() => onScript(node, tpl, copy)}>
                      {t(`script.${tpl}`)}
                    </ContextMenuItem>
                  ))}
                </ContextMenuSubContent>
              </ContextMenuSub>
            ))}
          </>
        )}
      </ContextMenuContent>
    </ContextMenu>
  );
});

function PagingRowView({
  row,
  onLoadMore,
  onClearPattern,
}: {
  row: PagingRow;
  onLoadMore: (path: ObjectPath) => void;
  onClearPattern: (path: ObjectPath) => void;
}) {
  const { t } = useTranslation('explorer');
  const pad = 4 + row.depth * INDENT + 20;
  const { paging } = row;
  if (row.type === 'pattern') {
    return (
      <div
        style={{ paddingLeft: pad, height: ROW_HEIGHT }}
        className="flex items-center gap-1 pr-2 text-xs text-muted-foreground"
      >
        <span className="min-w-0 truncate" title={paging.pattern}>
          {t('pattern.active', { pattern: paging.pattern })}
        </span>
        <button
          type="button"
          aria-label={t('pattern.clear')}
          title={t('pattern.clear')}
          onClick={() => onClearPattern(row.path)}
          className="shrink-0 rounded p-0.5 hover:bg-surface-muted hover:text-foreground"
        >
          <X size={12} />
        </button>
      </div>
    );
  }
  const remaining = paging.total !== null ? Math.max(0, paging.total - row.loaded) : null;
  return (
    <div
      style={{ paddingLeft: pad, height: ROW_HEIGHT }}
      className="flex items-center gap-2 pr-2 text-xs"
    >
      <button
        type="button"
        disabled={paging.loadingMore}
        onClick={() => onLoadMore(row.path)}
        className="flex items-center gap-1 rounded px-1 text-accent hover:bg-surface-muted disabled:opacity-60"
      >
        {paging.loadingMore && <Spinner size={11} />}
        {remaining !== null && remaining > 0
          ? t('loadMoreCount', { count: remaining })
          : t('loadMore')}
      </button>
      {paging.moreError && (
        <span
          role="alert"
          className="min-w-0 truncate text-danger"
          title={paging.moreError.message}
        >
          {t('loadError')}: {paging.moreError.message}
        </span>
      )}
    </div>
  );
}

function StatusRowView({ row, onRetry }: { row: StatusRow; onRetry: (path: ObjectPath) => void }) {
  const { t } = useTranslation('explorer');
  const pad = 4 + row.depth * INDENT + 20;
  if (row.type === 'empty') {
    return (
      <div
        style={{ paddingLeft: pad, height: ROW_HEIGHT }}
        className="flex items-center text-xs italic text-muted-foreground"
      >
        {t('empty')}
      </div>
    );
  }
  return (
    <div
      role="alert"
      style={{ paddingLeft: pad, height: ROW_HEIGHT }}
      className="flex items-center gap-2 pr-2 text-xs text-danger"
    >
      <span className="min-w-0 truncate" title={row.error?.message}>
        {t('loadError')}
        {row.error ? `: ${row.error.message}` : ''}
      </span>
      <button
        type="button"
        onClick={() => onRetry(row.path)}
        className="shrink-0 rounded px-1 text-foreground underline hover:bg-surface-muted"
      >
        {t('retry')}
      </button>
    </div>
  );
}

export type { TreeRow };
