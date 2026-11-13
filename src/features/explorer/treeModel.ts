import type { TFunction } from 'i18next';
import { lastSegment, pathKey, ROOT_PATH } from '@/lib/objectPath';
import {
  displayKeyName,
  displayKeyNameRelative,
  isBinaryKeyName,
} from '@/features/keyvalue/keyDetail';
import type { Capabilities, IpcError, ObjectKind, ObjectNode, ObjectPath } from '@/lib/types';
import type { PagingState } from './paging';

/** Load state of the children of one path, as read from the query cache. */
export type ChildrenState =
  | { status: 'idle' }
  | { status: 'loading' }
  | { status: 'error'; error: IpcError }
  | { status: 'loaded'; nodes: ObjectNode[] };

export interface NodeRow {
  type: 'node';
  key: string;
  node: ObjectNode;
  depth: number;
  /** Children are shown (explicitly expanded, or auto-expanded by a filter match). */
  expanded: boolean;
  /** Children are being fetched. */
  loading: boolean;
  parentKey: string | null;
}

/** Informational row below a node (or at the root): load error or empty folder. */
export interface StatusRow {
  type: 'error' | 'empty';
  key: string;
  depth: number;
  /** Key of the owning node; `null` for the root level. */
  parentKey: string | null;
  path: ObjectPath;
  error?: IpcError;
}

/** Row below the loaded children of a paged level: "load more" and the active glob filter. */
export interface PagingRow {
  type: 'more' | 'pattern';
  key: string;
  depth: number;
  parentKey: string | null;
  path: ObjectPath;
  paging: PagingState;
  /** Children loaded so far (`more` rows). */
  loaded: number;
}

export type TreeRow = NodeRow | StatusRow | PagingRow;

export function isPagingRow(row: TreeRow): row is PagingRow {
  return row.type === 'more' || row.type === 'pattern';
}

export interface FlattenOptions {
  getChildren: (path: ObjectPath) => ChildrenState;
  expanded: ReadonlySet<string>;
  /** Case-insensitive substring filter over loaded nodes; empty disables filtering. */
  filter?: string;
  /** Display label of a node (translated folder names). */
  label: (node: ObjectNode) => string;
  /** Paging state of a level; only paged levels have one. */
  getPaging?: (path: ObjectPath) => PagingState | undefined;
}

export const ROOT_KEY = pathKey(ROOT_PATH);

export function normalizeFilter(filter: string | undefined): string {
  return (filter ?? '').trim().toLowerCase();
}

/** True when `node` or any already-loaded descendant matches the (normalized) filter. */
export function subtreeMatches(
  node: ObjectNode,
  filter: string,
  getChildren: FlattenOptions['getChildren'],
  label: FlattenOptions['label'],
): boolean {
  if (label(node).toLowerCase().includes(filter)) return true;
  const state = getChildren(node.path);
  return (
    state.status === 'loaded' &&
    state.nodes.some((child) => subtreeMatches(child, filter, getChildren, label))
  );
}

/** Flatten the loaded part of the tree into the rows currently visible, depth first. */
export function flattenTree(options: FlattenOptions): TreeRow[] {
  const { getChildren, expanded, label } = options;
  const filter = normalizeFilter(options.filter);
  const rows: TreeRow[] = [];

  const visit = (path: ObjectPath, depth: number, parentKey: string | null) => {
    const state = getChildren(path);
    if (state.status === 'error') {
      if (!filter) {
        rows.push({
          type: 'error',
          key: `error:${parentKey ?? ROOT_KEY}`,
          depth,
          parentKey,
          path,
          error: state.error,
        });
      }
      return;
    }
    if (state.status !== 'loaded') return;
    const paging = filter ? undefined : options.getPaging?.(path);
    const scope = parentKey ?? ROOT_KEY;
    if (paging?.pattern) {
      rows.push({
        type: 'pattern',
        key: `pattern:${scope}`,
        depth,
        parentKey,
        path,
        paging,
        loaded: 0,
      });
    }
    if (state.nodes.length === 0 && !paging?.nextCursor) {
      if (!filter) {
        rows.push({ type: 'empty', key: `empty:${scope}`, depth, parentKey, path });
      }
      return;
    }
    for (const node of state.nodes) {
      if (filter && !subtreeMatches(node, filter, getChildren, label)) continue;
      const key = pathKey(node.path);
      const childState = getChildren(node.path);
      const isExpanded = filter
        ? node.hasChildren && childState.status === 'loaded'
        : node.hasChildren && expanded.has(key);
      rows.push({
        type: 'node',
        key,
        node,
        depth,
        expanded: isExpanded,
        loading: isExpanded && childState.status === 'loading',
        parentKey,
      });
      if (isExpanded) visit(node.path, depth + 1, key);
    }
    if (paging?.nextCursor) {
      rows.push({
        type: 'more',
        key: `more:${scope}`,
        depth,
        parentKey,
        path,
        paging,
        loaded: state.nodes.length,
      });
    }
  };

  visit(ROOT_PATH, 0, null);
  return rows;
}

export type NavKey = 'ArrowUp' | 'ArrowDown' | 'ArrowLeft' | 'ArrowRight' | 'Home' | 'End';

export interface NavResult {
  focusKey: string | null;
  /** Expand or collapse the node with this key. */
  toggle?: { key: string; expand: boolean };
}

/** Pure keyboard navigation over the flattened rows (status rows are skipped). */
export function navigate(rows: TreeRow[], focusKey: string | null, key: NavKey): NavResult {
  const nodes = rows.filter((r): r is NodeRow => r.type === 'node');
  if (nodes.length === 0) return { focusKey: null };
  const index = nodes.findIndex((r) => r.key === focusKey);
  const current = index >= 0 ? nodes[index]! : undefined;
  const clamp = (i: number) => nodes[Math.min(nodes.length - 1, Math.max(0, i))]!.key;

  switch (key) {
    case 'ArrowDown':
      return { focusKey: current ? clamp(index + 1) : nodes[0]!.key };
    case 'ArrowUp':
      return { focusKey: current ? clamp(index - 1) : nodes[0]!.key };
    case 'Home':
      return { focusKey: nodes[0]!.key };
    case 'End':
      return { focusKey: nodes[nodes.length - 1]!.key };
    case 'ArrowRight': {
      if (!current) return { focusKey: nodes[0]!.key };
      if (!current.node.hasChildren) return { focusKey };
      if (!current.expanded) return { focusKey, toggle: { key: current.key, expand: true } };
      const next = nodes[index + 1];
      return { focusKey: next && next.parentKey === current.key ? next.key : focusKey };
    }
    case 'ArrowLeft': {
      if (!current) return { focusKey: nodes[0]!.key };
      if (current.node.hasChildren && current.expanded) {
        return { focusKey, toggle: { key: current.key, expand: false } };
      }
      return { focusKey: current.parentKey ?? focusKey };
    }
  }
}

// ---------------------------------------------------------------------------------------------
// Activation and context menu
// ---------------------------------------------------------------------------------------------

const KEY_KINDS: ReadonlySet<ObjectKind> = new Set(['key']);
const TABLE_KINDS: ReadonlySet<ObjectKind> = new Set(['table', 'view', 'materializedView']);
const DDL_KINDS: ReadonlySet<ObjectKind> = new Set([
  'function',
  'procedure',
  'trigger',
  'sequence',
  'index',
]);

/** Driver capability an object kind depends on, when there is a flag for it. */
const KIND_CAPABILITY: Partial<Record<ObjectKind, keyof Capabilities>> = {
  view: 'views',
  materializedView: 'views',
  function: 'storedRoutines',
  procedure: 'storedRoutines',
  trigger: 'triggers',
  sequence: 'sequences',
  key: 'keyValueStore',
};

/** False only when the driver explicitly lacks the capability the kind needs. */
export function kindEnabled(kind: ObjectKind, caps?: Capabilities): boolean {
  const cap = KIND_CAPABILITY[kind];
  return !cap || !caps || caps[cap] !== false;
}

/** Which tab (if any) activating an object of `kind` opens. Unknown kinds open nothing. */
export function activationTab(
  kind: ObjectKind,
  caps?: Capabilities,
): 'table' | 'ddl' | 'key' | null {
  if (!kindEnabled(kind, caps)) return null;
  if (KEY_KINDS.has(kind)) return 'key';
  if (TABLE_KINDS.has(kind)) return 'table';
  if (DDL_KINDS.has(kind)) return 'ddl';
  return null;
}

export type NodeAction =
  | 'openData'
  | 'openKey'
  | 'viewDdl'
  | 'copyName'
  | 'copyFullName'
  | 'filterPattern'
  | 'deleteByPattern'
  | 'refresh';

/** Context-menu actions that apply to a node of `kind`, in display order. */
export function nodeActions(
  node: Pick<ObjectNode, 'kind' | 'hasChildren'> & { name?: string },
  caps?: Capabilities,
): NodeAction[] {
  const actions: NodeAction[] = [];
  if (!kindEnabled(node.kind, caps)) return node.hasChildren ? ['refresh'] : [];
  if (KEY_KINDS.has(node.kind)) actions.push('openKey');
  if (TABLE_KINDS.has(node.kind)) actions.push('openData');
  if (TABLE_KINDS.has(node.kind) || DDL_KINDS.has(node.kind)) actions.push('viewDdl');
  if (node.kind !== 'folder') actions.push('copyName');
  if (TABLE_KINDS.has(node.kind) || DDL_KINDS.has(node.kind) || node.kind === 'column') {
    actions.push('copyFullName');
  }
  const keyLevel = node.kind === 'keyPrefix' || (node.kind === 'database' && caps?.keyValueStore);
  if (keyLevel && caps?.pagedTree) actions.push('filterPattern');
  // A binary prefix cannot be written as a glob: no delete-by-pattern for it.
  if (keyLevel && caps?.keyValueStore && !isBinaryKeyName(node.name ?? '')) {
    actions.push('deleteByPattern');
  }
  if (node.hasChildren) actions.push('refresh');
  return actions;
}

/** `schema.table` (or `database.table` for schema-less servers); plain name for top objects. */
export function qualifiedName(path: ObjectPath): string {
  const last = lastSegment(path);
  if (!last) return '';
  const schema = path.segments.find((s) => s.kind === 'schema');
  const database = path.segments.find((s) => s.kind === 'database');
  const qualifier = schema ?? database;
  if (last.kind === 'column') {
    const parent = path.segments.filter((s) => s.kind !== 'folder').slice(0, -1);
    return [...parent.slice(-2).map((s) => s.name), last.name].join('.');
  }
  return qualifier && qualifier !== last ? `${qualifier.name}.${last.name}` : last.name;
}

/** Display label of a node: folders are translated, everything else shows its name. */
export function nodeLabel(
  t: TFunction,
  node: Pick<ObjectNode, 'kind' | 'name'> & { path?: ObjectPath },
): string {
  if (node.kind === 'folder') return t(`explorer:folder.${node.name}`, node.name);
  if (node.kind === 'key' || node.kind === 'keyPrefix') {
    // Inside a prefix folder only the part after the folder's name is interesting.
    const segments = node.path?.segments ?? [];
    const parent = segments[segments.length - 2];
    return parent?.kind === 'keyPrefix'
      ? displayKeyNameRelative(node.name, parent.name)
      : displayKeyName(node.name);
  }
  return node.name;
}
