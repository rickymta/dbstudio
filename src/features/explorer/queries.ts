import { useQueryClient, type QueryClient } from '@tanstack/react-query';
import { useEffect, useReducer } from 'react';
import { ipc } from '@/lib/ipc';
import { pathKey, ROOT_PATH } from '@/lib/objectPath';
import type { IpcError, ObjectNode, ObjectPath, Uuid } from '@/lib/types';
import { subtreeMatches, type ChildrenState, type FlattenOptions } from './treeModel';

const ROOT = 'objectChildren';

/** Query key of the children of `path` in the session's object tree. */
export const objectChildrenKey = (sessionId: Uuid, path: ObjectPath) =>
  [ROOT, sessionId, pathKey(path)] as const;

/** Load (or reuse the cached) children of a path. The error state stays in the cache. */
export function loadChildren(
  qc: QueryClient,
  sessionId: Uuid,
  path: ObjectPath,
): Promise<ObjectNode[]> {
  return qc.fetchQuery({
    queryKey: objectChildrenKey(sessionId, path),
    queryFn: () => ipc.objectChildren(sessionId, path),
    staleTime: Infinity,
    // Keep nodes cached while branches are collapsed; cleared by `clearSessionCache`.
    gcTime: Infinity,
  });
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
