import { RawNumber } from './format';

export const MAX_CHILDREN = 500;
/** "Expand all" opens at most this many nodes so a huge document cannot freeze the UI. */
export const MAX_EXPANDED_NODES = 5000;

export function isContainer(v: unknown): v is Record<string, unknown> | unknown[] {
  return typeof v === 'object' && v !== null && !(v instanceof RawNumber);
}

export function childEntries(v: Record<string, unknown> | unknown[]): [string, unknown][] {
  return Array.isArray(v)
    ? v.map((x, i) => [String(i), x] as [string, unknown])
    : Object.entries(v);
}

/**
 * Paths of the container nodes opened by "expand all", breadth first, up to `limit`.
 * Paths are the keys joined with `\u0000`; the root is the empty string.
 */
export function collectExpandPaths(
  root: unknown,
  limit = MAX_EXPANDED_NODES,
): { paths: Set<string>; truncated: boolean } {
  const paths = new Set<string>();
  const queue: [string, unknown][] = [['', root]];
  for (let i = 0; i < queue.length; i++) {
    const [path, node] = queue[i]!;
    if (!isContainer(node)) continue;
    if (paths.size >= limit) return { paths, truncated: true };
    paths.add(path);
    for (const [k, v] of childEntries(node).slice(0, MAX_CHILDREN)) {
      if (isContainer(v)) queue.push([`${path}\u0000${k}`, v]);
    }
  }
  return { paths, truncated: false };
}
