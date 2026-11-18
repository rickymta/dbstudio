import type { KeyboardEvent } from 'react';
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
import { activationTab } from '@/features/explorer/treeModel';
import type { ObjectNode, SessionInfo, Uuid } from '@/lib/types';
import { describeError } from './errors';
import { useConnectionsSnapshot } from './queries';
import { moveFocus } from './rowNav';
import { useServerSearch } from './useServerSearch';
import {
  extraDetail,
  resultContext,
  resultContextParts,
  searchScopeDatabase,
  SERVER_SEARCH_MIN_CHARS,
} from './serverSearchModel';

/**
 * "Results from the server" (BROWSE-06): one `object_search` per capable open session, grouped by
 * connection. `query` must already be debounced; a changed query uses a new cache key, so stale
 * answers are never shown.
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
        const scopeDb = searchScopeDatabase(
          session,
          snapshot?.profiles.find((p) => p.id === session.connectionId)?.database,
        );
        return (
          <div key={session.sessionId} role="none">
            <div className="flex items-center gap-1.5 px-3 py-0.5 text-xs font-medium">
              <span className="min-w-0 truncate">{name}</span>
              {result?.fetching && <Spinner size={11} />}
              {result?.data && <span className="text-muted-foreground">({nodes.length})</span>}
            </div>
            {result?.error && (
              <p role="alert" className="px-5 py-0.5 text-xs text-danger">
                {describeError(t, result.error)}
              </p>
            )}
            {result?.data && scopeDb && (
              <p className="px-5 py-0.5 text-xs text-muted-foreground">
                {t('search.currentDbOnly', { database: scopeDb })}
              </p>
            )}
            {result?.data && nodes.length === 0 && (
              <p className="px-5 py-0.5 text-xs italic text-muted-foreground">
                {t('search.noServerResults')}
              </p>
            )}
            {nodes.map((node) => (
              <ResultRow
                key={node.path.segments.map((s) => `${s.kind}:${s.name}`).join('/')}
                node={node}
                session={session}
                connectionName={name}
                query={query}
                onShowInTree={onShowInTree}
              />
            ))}
          </div>
        );
      })}
    </section>
  );
}

function ResultRow({
  node,
  session,
  connectionName,
  query,
  onShowInTree,
}: {
  node: ObjectNode;
  session: SessionInfo;
  connectionName: string;
  query: string;
  onShowInTree: (connectionId: Uuid, node: ObjectNode) => void;
}) {
  const { t } = useTranslation('connections');
  const openable = activationTab(node.kind, session.descriptor.capabilities) !== null;
  const context = resultContext(node);
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
          style={{ paddingLeft: 20 }}
          className="flex h-[26px] cursor-default items-center gap-1 pr-2 text-[13px] outline-none hover:bg-surface-muted focus-visible:bg-accent/20"
        >
          <KindIcon kind={node.kind} className="shrink-0 text-muted-foreground" />
          <span className="min-w-0 truncate">
            <Highlight text={node.name} query={query} />
          </span>
          {context && (
            <span className="min-w-0 truncate text-[11px] text-muted-foreground">{context}</span>
          )}
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
