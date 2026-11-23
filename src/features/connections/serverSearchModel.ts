import { useMemo } from 'react';
import { pathKey } from '@/lib/objectPath';
import {
  FOLDER_KEYS,
  type ObjectKind,
  type ObjectNode,
  type ObjectPath,
  type SessionInfo,
} from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';

export const SERVER_SEARCH_MIN_CHARS = 2;
/**
 * Results asked from each connection. They come best match first, whatever their kind, and are
 * then shown by kind: with a small limit one kind with many matches (tables named `user...`)
 * left no room for the others (the procedures). An answer of exactly this size is reported as
 * cut off.
 */
export const SERVER_SEARCH_LIMIT = 200;

/** Order of the kinds in a list of results; kinds not named here follow, as they come. */
export const RESULT_KIND_ORDER: readonly ObjectKind[] = [
  'table',
  'view',
  'materializedView',
  'collection',
  'function',
  'procedure',
  'trigger',
  'sequence',
  'synonym',
  'type',
  'package',
  'dictionary',
];

export interface ResultGroup {
  kind: ObjectKind;
  nodes: ObjectNode[];
}

/**
 * Results split by kind (tables, views, functions, procedures...), in a fixed order. Only kinds
 * with results get a group; inside a group the order of the server (best match first) is kept.
 */
export function groupResultsByKind(nodes: readonly ObjectNode[]): ResultGroup[] {
  const byKind = new Map<ObjectKind, ObjectNode[]>();
  for (const node of nodes) {
    const list = byKind.get(node.kind);
    if (list) list.push(node);
    else byKind.set(node.kind, [node]);
  }
  const position = (kind: ObjectKind) => {
    const i = RESULT_KIND_ORDER.indexOf(kind);
    return i === -1 ? RESULT_KIND_ORDER.length : i;
  };
  return [...byKind.entries()]
    .map(([kind, list]) => ({ kind, nodes: list }))
    .sort((a, b) => position(a.kind) - position(b.kind));
}

const KIND_FOLDER: Partial<Record<ObjectKind, string>> = {
  table: 'tables',
  view: 'views',
  materializedView: 'materializedViews',
  collection: 'collections',
  function: 'functions',
  procedure: 'procedures',
  trigger: 'triggers',
  sequence: 'sequences',
  synonym: 'synonyms',
  type: 'types',
  package: 'packages',
  dictionary: 'dictionaries',
};

/** Key (namespace `explorer`) of the plural name of a kind, the one its folder has in the tree. */
export function kindGroupLabelKey(kind: ObjectKind): string {
  const folder = KIND_FOLDER[kind];
  return folder ? `explorer:folder.${folder}` : `explorer:kind.${kind}`;
}

/** Open sessions whose driver can search on the server (capability-driven, no DB names). */
export function useSearchableSessions(): SessionInfo[] {
  const sessions = useSessionsStore((s) => s.sessionsByConnection);
  return useMemo(
    () => Object.values(sessions).filter((s) => s.descriptor.capabilities.objectSearch),
    [sessions],
  );
}

/** Names above a result: its path without folder segments and without the object itself. */
export function resultContextParts(node: Pick<ObjectNode, 'path'>): string[] {
  return node.path.segments
    .slice(0, -1)
    .filter((s) => s.kind !== 'folder')
    .map((s) => s.name);
}

/** Display context of a result, e.g. `db › schema`. */
export function resultContext(node: Pick<ObjectNode, 'path'>): string {
  return resultContextParts(node).join(' › ');
}

/**
 * The part of the backend `detail` that is not already said by the context. Drivers put the
 * location (`db.schema`, `schema`, a database name) first and may append more (`partitioned`).
 */
export function extraDetail(node: Pick<ObjectNode, 'path' | 'detail'>): string {
  let detail = (node.detail ?? '').trim();
  if (!detail) return '';
  const parts = resultContextParts(node);
  const known = [parts.join('.'), parts.join(' › '), parts.at(-1) ?? ''].filter(Boolean);
  for (const c of known) {
    if (!detail.toLowerCase().startsWith(c.toLowerCase())) continue;
    const rest = detail.slice(c.length);
    if (rest === '' || /^[\s,;·-]/.test(rest)) {
      detail = rest.replace(/^[\s,;·-]+/, '');
      break;
    }
  }
  return detail;
}

/** A node of the tree of results: a level of the object tree (database, schema, folder) or a match. */
export interface ResultTreeNode {
  /** Unique in the tree: the key of `path`. */
  key: string;
  kind: ObjectKind;
  name: string;
  path: ObjectPath;
  /** The result itself when this node is one (levels above a result have none). */
  match?: ObjectNode;
  children: ResultTreeNode[];
  /** Results in this node and below. */
  count: number;
}

/**
 * The results laid out like the object tree: database, schema, folder of the kind (tables,
 * functions, procedures...), object, following the path of each result. Levels without a result
 * are left out, except the databases in `databases` (the ones the tree already lists): they
 * are all kept, empty ones included, so that it shows where nothing matched.
 *
 * Folders come in the order of the tree, databases and schemas by name, and the results of a
 * folder as the server sent them (best match first).
 */
export function buildResultTree(
  nodes: readonly ObjectNode[],
  databases: readonly Pick<ObjectNode, 'kind' | 'name' | 'path'>[] = [],
): ResultTreeNode[] {
  const roots: ResultTreeNode[] = [];
  const child = (list: ResultTreeNode[], path: ObjectPath): ResultTreeNode => {
    const key = pathKey(path);
    let found = list.find((n) => n.key === key);
    if (!found) {
      const last = path.segments[path.segments.length - 1]!;
      found = { key, kind: last.kind, name: last.name, path, children: [], count: 0 };
      list.push(found);
    }
    return found;
  };
  for (const node of nodes) {
    let level = roots;
    node.path.segments.forEach((_, i) => {
      const at = child(level, { segments: node.path.segments.slice(0, i + 1) });
      at.count += 1;
      if (i === node.path.segments.length - 1) at.match = node;
      level = at.children;
    });
  }
  // Only when the results sit under databases: a tree without that level has nothing to add.
  if (roots.every((n) => n.kind === 'database')) {
    for (const db of databases) if (db.kind === 'database') child(roots, db.path);
  }
  const folder = (n: ResultTreeNode) => FOLDER_KEYS.indexOf(n.name as (typeof FOLDER_KEYS)[number]);
  const level = (n: ResultTreeNode) => !n.match && (n.kind === 'database' || n.kind === 'schema');
  const sort = (list: ResultTreeNode[]) => {
    list.sort((a, b) => {
      if (a.kind === 'folder' && b.kind === 'folder') return folder(a) - folder(b);
      if (level(a) && level(b)) return a.name.localeCompare(b.name);
      return 0;
    });
    list.forEach((n) => sort(n.children));
  };
  sort(roots);
  return roots;
}
