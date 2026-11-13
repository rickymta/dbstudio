import { mockChildren } from './catalog';
import { ROOT_PATH } from '../objectPath';
import type { DbKind, ObjectKind, ObjectNode, ObjectPath, ObjectSearchRequest } from '../types';
import { pathsEqual } from '../objectPath';

/** Kinds that are leaves for searching: their children (columns, indexes) are not searched. */
const LEAF_KINDS: ReadonlySet<ObjectKind> = new Set([
  'table',
  'view',
  'materializedView',
  'function',
  'procedure',
  'trigger',
  'sequence',
  'index',
  'role',
  'column',
  'collection',
  'key',
]);
/** Folders and other pure navigation nodes never match. */
const SKIP_KINDS: ReadonlySet<ObjectKind> = new Set(['folder']);

function collect(kind: DbKind, path: ObjectPath, out: ObjectNode[]): void {
  for (const child of mockChildren(kind, path)) {
    if (!SKIP_KINDS.has(child.kind)) out.push(child);
    if (!LEAF_KINDS.has(child.kind) && child.hasChildren) collect(kind, child.path, out);
  }
}

function startsWithPath(path: ObjectPath, scope: ObjectPath): boolean {
  return (
    path.segments.length >= scope.segments.length &&
    pathsEqual({ segments: path.segments.slice(0, scope.segments.length) }, scope)
  );
}

/** Rank 0 = exact, 1 = prefix, 2 = substring (case-insensitive); `null` = no match. */
function rank(name: string, query: string): number | null {
  const n = name.toLowerCase();
  if (n === query) return 0;
  if (n.startsWith(query)) return 1;
  return n.includes(query) ? 2 : null;
}

/** Like the real drivers: the location of the object (`db.schema`, or the database name). */
function searchDetail(node: ObjectNode): string | null {
  const parts = node.path.segments
    .slice(0, -1)
    .filter((s) => s.kind !== 'folder')
    .map((s) => s.name);
  return parts.length ? parts.join('.') : null;
}

/** Search the fake catalog the way the real drivers do: exact, then prefix, then substring. */
export function mockObjectSearch(kind: DbKind, request: ObjectSearchRequest): ObjectNode[] {
  const query = request.query.trim().toLowerCase();
  if (!query) return [];
  const all: ObjectNode[] = [];
  collect(kind, ROOT_PATH, all);
  return all
    .filter((n) => startsWithPath(n.path, request.scope))
    .filter((n) => !request.kinds || request.kinds.includes(n.kind))
    .flatMap((n) => {
      const r = rank(n.name, query);
      return r === null ? [] : [{ n, r }];
    })
    .sort((a, b) => a.r - b.r || a.n.name.localeCompare(b.n.name))
    .slice(0, Math.max(0, request.limit))
    .map((x) => ({ ...x.n, detail: searchDetail(x.n) }));
}
