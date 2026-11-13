import { useQueryClient, type QueryClient } from '@tanstack/react-query';
import { useEffect, useReducer } from 'react';
import { ipc, toIpcError } from '@/lib/ipc';
import { pathKey, ROOT_PATH } from '@/lib/objectPath';
import type { IpcError, ObjectNode, ObjectPath, Uuid } from '@/lib/types';
import { dedupeNodes, mergePage, TREE_PAGE_SIZE, type PagingState } from './paging';
import { subtreeMatches, type ChildrenState, type FlattenOptions } from './treeModel';

const ROOT = 'objectChildren';

/** Query key of the children of `path` in the session's object tree. */
export const objectChildrenKey = (sessionId: Uuid, path: ObjectPath) =>
  [ROOT, sessionId, pathKey(path)] as const;

/** Query key of the paging state of a (paged) level. */
export const pagingKey = (sessionId: Uuid, path: ObjectPath) =>
  [ROOT, sessionId, pathKey(path), 'paging'] as const;

/** Glob filters set on tree levels, by session and level; they survive a branch refresh. */
const levelPatterns = new Map<string, string>();
const patternId = (sessionId: Uuid, path: ObjectPath) => `${sessionId}\u0000${pathKey(path)}`;

export function getLevelPattern(sessionId: Uuid, path: ObjectPath): string {
  return levelPatterns.get(patternId(sessionId, path)) ?? '';
}

/** Keep paging state and nodes cached while branches are collapsed. */
function keepForever(qc: QueryClient): void {
  qc.setQueryDefaults([ROOT], { staleTime: Infinity, gcTime: Infinity });
}

export function readPaging(
  qc: QueryClient,
  sessionId: Uuid,
  path: ObjectPath,
): PagingState | undefined {
  return qc.getQueryData<PagingState>(pagingKey(sessionId, path));
}

function writePaging(qc: QueryClient, sessionId: Uuid, path: ObjectPath, state: PagingState) {
  qc.setQueryData(pagingKey(sessionId, path), state);
}

async function fetchFirstPage(
  qc: QueryClient,
  sessionId: Uuid,
  path: ObjectPath,
): Promise<ObjectNode[]> {
  const pattern = getLevelPattern(sessionId, path);
  const page = await ipc.objectChildrenPage(sessionId, {
    parent: path,
    cursor: null,
    limit: TREE_PAGE_SIZE,
    pattern: pattern || null,
  });
  writePaging(qc, sessionId, path, {
    nextCursor: page.nextCursor,
    pattern,
    total: page.approximateTotal,
    loadingMore: false,
  });
  return dedupeNodes(page.nodes);
}

/**
 * Load (or reuse the cached) children of a path. The error state stays in the cache. With
 * `paged` the level is read page by page (`objectChildrenPage`): only the first page is loaded
 * here, `loadMoreChildren` appends the rest.
 */
export function loadChildren(
  qc: QueryClient,
  sessionId: Uuid,
  path: ObjectPath,
  paged = false,
): Promise<ObjectNode[]> {
  keepForever(qc);
  return qc.fetchQuery({
    queryKey: objectChildrenKey(sessionId, path),
    queryFn: () =>
      paged ? fetchFirstPage(qc, sessionId, path) : ipc.objectChildren(sessionId, path),
    staleTime: Infinity,
    // Keep nodes cached while branches are collapsed; cleared by `clearSessionCache`.
    gcTime: Infinity,
  });
}

/** Append the next page of a paged level; failures are kept in the paging state. */
export async function loadMoreChildren(
  qc: QueryClient,
  sessionId: Uuid,
  path: ObjectPath,
): Promise<void> {
  const paging = readPaging(qc, sessionId, path);
  if (!paging?.nextCursor || paging.loadingMore) return;
  const cursor = paging.nextCursor;
  writePaging(qc, sessionId, path, { ...paging, loadingMore: true, moreError: undefined });
  try {
    const page = await ipc.objectChildrenPage(sessionId, {
      parent: path,
      cursor,
      limit: TREE_PAGE_SIZE,
      pattern: paging.pattern || null,
    });
    // The level was refreshed or re-filtered while the request ran: drop the stale page.
    if (readPaging(qc, sessionId, path)?.nextCursor !== cursor) return;
    const current = qc.getQueryData<ObjectNode[]>(objectChildrenKey(sessionId, path)) ?? [];
    qc.setQueryData(objectChildrenKey(sessionId, path), mergePage(current, page.nodes));
    writePaging(qc, sessionId, path, {
      ...paging,
      nextCursor: page.nextCursor,
      total: page.approximateTotal ?? paging.total,
      loadingMore: false,
      moreError: undefined,
    });
  } catch (e) {
    if (readPaging(qc, sessionId, path)?.nextCursor !== cursor) return;
    writePaging(qc, sessionId, path, { ...paging, loadingMore: false, moreError: toIpcError(e) });
  }
}

/** Change the glob filter of a level (empty clears it) and drop its cached children. */
export function setLevelPattern(
  qc: QueryClient,
  sessionId: Uuid,
  path: ObjectPath,
  pattern: string,
): void {
  const trimmed = pattern.trim();
  if (trimmed) levelPatterns.set(patternId(sessionId, path), trimmed);
  else levelPatterns.delete(patternId(sessionId, path));
  clearBranch(qc, sessionId, path);
}

export function readChildrenState(
  qc: QueryClient,
  sessionId: Uuid,
  path: ObjectPath,
): ChildrenState {
  const state = qc.getQueryState<ObjectNode[], IpcError>(objectChildrenKey(sessionId, path));
  if (!state) return { status: 'idle' };
  if (state.data) return { status: 'loaded', nodes: state.data };
  if (state.fetchStatus === 'fetching') return { status: 'loading' };
  if (state.status === 'error' && state.error) return { status: 'error', error: state.error };
  return { status: 'idle' };
}

function isUnder(candidate: unknown, prefix: ObjectPath): boolean {
  if (typeof candidate !== 'string') return false;
  let segments: unknown;
  try {
    segments = JSON.parse(candidate);
  } catch {
    return false;
  }
  if (!Array.isArray(segments) || segments.length < prefix.segments.length) return false;
  return prefix.segments.every((s, i) => {
    const seg = segments[i] as unknown;
    return Array.isArray(seg) && seg[0] === s.kind && seg[1] === s.name;
  });
}

/** Drop the cached children of `path` and everything below it (refresh of a branch). */
export function clearBranch(qc: QueryClient, sessionId: Uuid, path: ObjectPath): void {
  qc.removeQueries({
    predicate: (q) =>
      q.queryKey[0] === ROOT && q.queryKey[1] === sessionId && isUnder(q.queryKey[2], path),
  });
}

/** Drop every cached node of a session (session closed). */
export function clearSessionCache(qc: QueryClient, sessionId: Uuid): void {
  for (const id of [...levelPatterns.keys()]) {
    if (id.startsWith(`${sessionId}\u0000`)) levelPatterns.delete(id);
  }
  clearBranch(qc, sessionId, ROOT_PATH);
}

/** Re-render when anything cached for the session's tree changes; returns a change counter. */
export function useTreeCacheVersion(sessionId: Uuid | undefined): number {
  const qc = useQueryClient();
  const [version, bump] = useReducer((n: number) => n + 1, 0);
  useEffect(() => {
    if (!sessionId) return;
    return qc.getQueryCache().subscribe((event) => {
      const key = event.query.queryKey;
      if (key[0] === ROOT && key[1] === sessionId) bump();
    });
  }, [qc, sessionId]);
  return version;
}

/** True when some already-loaded node of the session's tree matches the filter. */
export function treeHasMatch(
  qc: QueryClient,
  sessionId: Uuid,
  filter: string,
  label: FlattenOptions['label'],
): boolean {
  const getChildren = (path: ObjectPath) => readChildrenState(qc, sessionId, path);
  const root = getChildren(ROOT_PATH);
  return (
    root.status === 'loaded' &&
    root.nodes.some((n) => subtreeMatches(n, filter.trim().toLowerCase(), getChildren, label))
  );
}

/** Like `useTreeCacheVersion` but for the trees of every session (used while filtering). */
export function useAnyTreeCacheVersion(enabled: boolean): number {
  const qc = useQueryClient();
  const [version, bump] = useReducer((n: number) => n + 1, 0);
  useEffect(() => {
    if (!enabled) return;
    return qc.getQueryCache().subscribe((event) => {
      if (event.query.queryKey[0] === ROOT) bump();
    });
  }, [qc, enabled]);
  return version;
}
