import { pathKey } from '@/lib/objectPath';
import type { IpcError, ObjectNode } from '@/lib/types';

/** Children requested per page of a paged tree level. */
export const TREE_PAGE_SIZE = 200;

/** Paging state of one tree level (kept in the query cache next to its nodes). */
export interface PagingState {
  /** `null` = every page is loaded. */
  nextCursor: string | null;
  /** Glob filter in effect for the level ("" = none). */
  pattern: string;
  /** Matching children on the server, when it knows. */
  total: number | null;
  loadingMore: boolean;
  moreError?: IpcError;
}

/**
 * Append a page to the nodes already loaded. Redis can return a `KeyPrefix` again on a later
 * page, so nodes whose path is already present are dropped (first occurrence wins).
 */
export function mergePage(existing: ObjectNode[], page: ObjectNode[]): ObjectNode[] {
  const seen = new Set(existing.map((n) => pathKey(n.path)));
  const added: ObjectNode[] = [];
  for (const node of page) {
    const key = pathKey(node.path);
    if (seen.has(key)) continue;
    seen.add(key);
    added.push(node);
  }
  return added.length === 0 ? existing : [...existing, ...added];
}

/** Same as `mergePage` for a first page (drops repeats inside the page). */
export function dedupeNodes(nodes: ObjectNode[]): ObjectNode[] {
  return mergePage([], nodes);
}
