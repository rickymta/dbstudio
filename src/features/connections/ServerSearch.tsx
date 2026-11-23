import { ChevronDown, ChevronRight } from 'lucide-react';
import { useQueries } from '@tanstack/react-query';
import { useState, type KeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuTrigger,
} from '@/components/ui/ContextMenu';
import { Highlight } from '@/components/ui/Highlight';
import { Spinner } from '@/components/ui/Spinner';
import { KindIcon } from '@/features/explorer/kindIcons';
import { openObjectTab } from '@/features/explorer/openObject';
import { activationTab, nodeLabel } from '@/features/explorer/treeModel';
import { cn } from '@/lib/cn';
import { ipc } from '@/lib/ipc';
import { ROOT_PATH } from '@/lib/objectPath';
import type { ObjectNode, SessionInfo, Uuid } from '@/lib/types';
import { describeError } from './errors';
import { useConnectionsSnapshot } from './queries';
import { moveFocus } from './rowNav';
import { useServerSearch } from './useServerSearch';
import {
  buildResultTree,
  extraDetail,
  resultContextParts,
  SERVER_SEARCH_LIMIT,
  SERVER_SEARCH_MIN_CHARS,
  type ResultTreeNode,
} from './serverSearchModel';

/** Indent of one level of the tree of results. */
const INDENT = 12;

/**
 * "Results from the server" (BROWSE-06): one `object_search` per capable open session, shown per
 * connection as the object tree itself (database, schema, folder of the kind, object) cut down
 * to the matches. `query` must already be debounced; a changed query uses a new cache key, so
 * stale answers are never shown.
 */
export function ServerSearch({
  query,
  sessions,
  onShowInTree,
}: {
  query: string;
  sessions: SessionInfo[];
  onShowInTree: (connectionId: Uuid, node: ObjectNode) => void;
}) {
  const { t } = useTranslation('connections');
  const { data: snapshot } = useConnectionsSnapshot();
  const enabled = query.length >= SERVER_SEARCH_MIN_CHARS;
  const results = useServerSearch(sessions, query, enabled);
  // Top level of each connection (its databases), so that all of them stay in sight, also the
  // ones without a match. Asked here and not read from the cache of the object tree: that cache
  // is dropped as soon as the filter hides the tree of the connection.
  const tops = useQueries({
    queries: sessions.map((session) => ({
      queryKey: ['serverSearchTopLevel', session.sessionId],
      queryFn: () => ipc.objectChildren(session.sessionId, ROOT_PATH),
      enabled,
      staleTime: 60_000,
      retry: false,
    })),
  });
  // Branches the user closed, by session and path; every branch starts open.
  const [closed, setClosed] = useState<ReadonlySet<string>>(() => new Set());
  const toggle = (key: string) =>
    setClosed((prev) => {
      const next = new Set(prev);
      if (!next.delete(key)) next.add(key);
      return next;
    });

  if (!enabled || sessions.length === 0) return null;

  return (
    <section
      role="group"
      aria-label={t('search.serverResults')}
      data-testid="server-results"
      className="mt-1 border-t border-border pt-1"
    >
      <h3 className="px-3 py-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
        {t('search.serverResults')}
      </h3>
      {sessions.map((session, i) => {
        const result = results[i];
        const name =
          snapshot?.profiles.find((p) => p.id === session.connectionId)?.name ??
          session.serverInfo.product;
        const nodes = result?.data ?? [];
        // A full answer was cut by the limit: say so, there may be more on the server.
        const cut = nodes.length >= SERVER_SEARCH_LIMIT;
        const tree = result?.data ? buildResultTree(nodes, tops[i]?.data ?? []) : [];
        return (
          <div key={session.sessionId} role="none">
            <div className="flex items-center gap-1.5 px-3 py-0.5 text-xs font-medium">
              <span className="min-w-0 truncate">{name}</span>
              {result?.fetching && <Spinner size={11} />}
              {result?.data && (
                <span className="text-muted-foreground">
                  ({nodes.length}
                  {cut ? '+' : ''})
                </span>
              )}
            </div>
            {result?.error && (
              <p role="alert" className="px-5 py-0.5 text-xs text-danger">
                {describeError(t, result.error)}
              </p>
            )}
            {result?.data && nodes.length === 0 && (
              <p className="px-5 py-0.5 text-xs italic text-muted-foreground">
                {t('search.noServerResults')}
              </p>
            )}
            {cut && (
              <p className="px-5 py-0.5 text-xs text-muted-foreground">
                {t('search.truncated', { count: nodes.length })}
              </p>
            )}
            {nodes.length > 0 && (
              <Branches
                nodes={tree}
                depth={0}
                session={session}
                connectionName={name}
                query={query}
                closed={closed}
                toggle={toggle}
                onShowInTree={onShowInTree}
              />
            )}
          </div>
        );
      })}
    </section>
  );
}

interface BranchProps {
  session: SessionInfo;
  connectionName: string;
  query: string;
  closed: ReadonlySet<string>;
  toggle: (key: string) => void;
  onShowInTree: (connectionId: Uuid, node: ObjectNode) => void;
}

/** One level of the tree of results: matches as rows, the levels above them as branches. */
function Branches({
  nodes,
  depth,
  ...rest
}: BranchProps & { nodes: ResultTreeNode[]; depth: number }) {
  const { t } = useTranslation('connections');
  const { session, connectionName, closed, toggle } = rest;
  return nodes.map((node) => {
    if (node.match && node.children.length === 0) {
      return (
        <ResultRow
          key={node.key}
          node={node.match}
          depth={depth}
          session={session}
          connectionName={connectionName}
          query={rest.query}
          onShowInTree={rest.onShowInTree}
        />
      );
    }
    const key = `${session.sessionId}:${node.key}`;
    const empty = node.count === 0;
    const open = !empty && !closed.has(key);
    const label = nodeLabel(t, node);
    return (
      <div key={node.key} role="none" data-testid="server-result-branch">
        <div
          role="treeitem"
          aria-expanded={empty ? undefined : open}
          aria-selected={false}
          aria-label={`${label} (${node.count}) - ${connectionName}`}
          tabIndex={0}
          data-nav-row
          onClick={() => !empty && toggle(key)}
          onKeyDown={(e) => {
            if (e.key === 'ArrowDown') moveFocus(e, 1);
            else if (e.key === 'ArrowUp') moveFocus(e, -1);
            else if (
              !empty &&
              (e.key === 'Enter' ||
                e.key === ' ' ||
                (e.key === 'ArrowRight' && !open) ||
                (e.key === 'ArrowLeft' && open))
            ) {
              e.preventDefault();
              toggle(key);
            }
          }}
          style={{ paddingLeft: 12 + depth * INDENT }}
          className={cn(
            'flex h-[24px] cursor-default items-center gap-1 pr-2 text-[13px] outline-none hover:bg-surface-muted focus-visible:bg-accent/20',
            empty && 'text-muted-foreground',
          )}
        >
          {empty ? (
            <span className="w-3 shrink-0" />
          ) : open ? (
            <ChevronDown size={12} aria-hidden className="shrink-0 text-muted-foreground" />
          ) : (
            <ChevronRight size={12} aria-hidden className="shrink-0 text-muted-foreground" />
          )}
          <KindIcon kind={node.kind} className="shrink-0 text-muted-foreground" />
          <span className="min-w-0 truncate">{label}</span>
          <span className="text-[11px] text-muted-foreground">({node.count})</span>
        </div>
        {open && <Branches nodes={node.children} depth={depth + 1} {...rest} />}
      </div>
    );
  });
}

function ResultRow({
  node,
  depth,
  session,
  connectionName,
  query,
  onShowInTree,
}: {
  node: ObjectNode;
  depth: number;
  session: SessionInfo;
  connectionName: string;
  query: string;
  onShowInTree: (connectionId: Uuid, node: ObjectNode) => void;
}) {
  const { t } = useTranslation('connections');
  const openable = activationTab(node.kind, session.descriptor.capabilities) !== null;
  const extra = extraDetail(node);
  const open = () => openObjectTab(node, session);
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'ArrowDown') moveFocus(e, 1);
    else if (e.key === 'ArrowUp') moveFocus(e, -1);
    else if (e.key === 'Enter') {
      e.preventDefault();
      if (openable) open();
      else onShowInTree(session.connectionId, node);
    }
  };
  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>
        <div
          role="treeitem"
          aria-selected={false}
          aria-label={`${node.name} - ${connectionName}`}
          tabIndex={0}
          data-nav-row
          data-testid="server-result"
          title={[...resultContextParts(node), node.name].join('.')}
          onDoubleClick={() => (openable ? open() : onShowInTree(session.connectionId, node))}
          onKeyDown={onKeyDown}
          style={{ paddingLeft: 12 + depth * INDENT + 16 }}
          className="flex h-[26px] cursor-default items-center gap-1 pr-2 text-[13px] outline-none hover:bg-surface-muted focus-visible:bg-accent/20"
        >
          <KindIcon kind={node.kind} className="shrink-0 text-muted-foreground" />
          <span className="min-w-0 truncate">
            <Highlight text={node.name} query={query} />
          </span>
          {extra && (
            <span className="ml-auto max-w-[40%] shrink-0 truncate pl-2 text-[11px] text-muted-foreground">
              {extra}
            </span>
          )}
        </div>
      </ContextMenuTrigger>
      <ContextMenuContent>
        {openable && (
          <ContextMenuItem onSelect={open}>
            {activationTab(node.kind, session.descriptor.capabilities) === 'table'
              ? t('search.openData')
              : t('search.viewDdl')}
          </ContextMenuItem>
        )}
        <ContextMenuItem onSelect={() => onShowInTree(session.connectionId, node)}>
          {t('search.showInTree')}
        </ContextMenuItem>
      </ContextMenuContent>
    </ContextMenu>
  );
}
