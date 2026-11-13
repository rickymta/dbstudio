import { describe, expect, it } from 'vitest';
import { childPath, pathKey, ROOT_PATH } from '@/lib/objectPath';
import type { ObjectKind, ObjectNode, ObjectPath } from '@/lib/types';
import {
  activationTab,
  flattenTree,
  navigate,
  nodeActions,
  qualifiedName,
  type ChildrenState,
  type TreeRow,
} from './treeModel';

function node(parent: ObjectPath, kind: ObjectKind, name: string, hasChildren = true): ObjectNode {
  return { path: childPath(parent, kind, name), kind, name, hasChildren };
}

const db = node(ROOT_PATH, 'database', 'app');
const schemas = node(db.path, 'folder', 'schemas');
const pub = node(schemas.path, 'schema', 'public');
const tables = node(pub.path, 'folder', 'tables');
const users = node(tables.path, 'table', 'users');
const orders = node(tables.path, 'table', 'orders');
const other = node(ROOT_PATH, 'database', 'other');

function tree(entries: [ObjectPath, ChildrenState][]) {
  const map = new Map(entries.map(([p, s]) => [pathKey(p), s]));
  return (p: ObjectPath): ChildrenState => map.get(pathKey(p)) ?? { status: 'idle' };
}

const loaded = (nodes: ObjectNode[]): ChildrenState => ({ status: 'loaded', nodes });
const label = (n: ObjectNode) => n.name;

const fullTree = tree([
  [ROOT_PATH, loaded([db, other])],
  [db.path, loaded([schemas])],
  [schemas.path, loaded([pub])],
  [pub.path, loaded([tables])],
  [tables.path, loaded([users, orders])],
]);

const keysOf = (rows: TreeRow[]) => rows.map((r) => (r.type === 'node' ? r.node.name : r.type));

describe('flattenTree', () => {
  it('shows only roots when nothing is expanded', () => {
    const rows = flattenTree({ getChildren: fullTree, expanded: new Set(), label });
    expect(keysOf(rows)).toEqual(['app', 'other']);
  });

  it('lists expanded branches depth first with depth and parent keys', () => {
    const expanded = new Set([db.path, schemas.path, pub.path, tables.path].map(pathKey));
    const rows = flattenTree({ getChildren: fullTree, expanded, label });
    expect(keysOf(rows)).toEqual([
      'app',
      'schemas',
      'public',
      'tables',
      'users',
      'orders',
      'other',
    ]);
    const usersRow = rows.find((r) => r.type === 'node' && r.node.name === 'users');
    expect(usersRow).toMatchObject({ depth: 4, parentKey: pathKey(tables.path), expanded: false });
  });

  it('adds loading, error and empty states', () => {
    const getChildren = tree([
      [ROOT_PATH, loaded([db, other])],
      [db.path, { status: 'loading' }],
      [other.path, { status: 'error', error: { kind: 'query', message: 'boom' } }],
    ]);
    const rows = flattenTree({
      getChildren,
      expanded: new Set([db.path, other.path].map(pathKey)),
      label,
    });
    expect(rows[0]).toMatchObject({ type: 'node', loading: true });
    expect(rows.map((r) => r.type)).toEqual(['node', 'node', 'error']);

    const empty = flattenTree({
      getChildren: tree([
        [ROOT_PATH, loaded([db])],
        [db.path, loaded([])],
      ]),
      expanded: new Set([pathKey(db.path)]),
      label,
    });
    expect(empty.map((r) => r.type)).toEqual(['node', 'empty']);
  });

  it('filters to matching nodes and keeps their ancestors, expanding them', () => {
    const rows = flattenTree({
      getChildren: fullTree,
      expanded: new Set(),
      filter: 'ORD',
      label,
    });
    expect(keysOf(rows)).toEqual(['app', 'schemas', 'public', 'tables', 'orders']);
    expect(rows.every((r) => r.type !== 'node' || r.depth >= 0)).toBe(true);
  });

  it('returns nothing when the filter matches nothing', () => {
    expect(
      flattenTree({ getChildren: fullTree, expanded: new Set(), filter: 'zzz', label }),
    ).toEqual([]);
  });
});

describe('navigate', () => {
  const expanded = new Set([db.path, schemas.path].map(pathKey));
  const rows = flattenTree({ getChildren: fullTree, expanded, label });
  // rows: app (expanded), schemas (expanded), public, other
  const k = (n: ObjectNode) => pathKey(n.path);

  it('moves up and down, clamped at the ends', () => {
    expect(navigate(rows, null, 'ArrowDown').focusKey).toBe(k(db));
    expect(navigate(rows, k(db), 'ArrowDown').focusKey).toBe(k(schemas));
    expect(navigate(rows, k(db), 'ArrowUp').focusKey).toBe(k(db));
    expect(navigate(rows, k(other), 'ArrowDown').focusKey).toBe(k(other));
    expect(navigate(rows, k(pub), 'Home').focusKey).toBe(k(db));
    expect(navigate(rows, k(db), 'End').focusKey).toBe(k(other));
  });

  it('right expands a collapsed node, then goes to its first child', () => {
    expect(navigate(rows, k(other), 'ArrowRight')).toEqual({
      focusKey: k(other),
      toggle: { key: k(other), expand: true },
    });
    expect(navigate(rows, k(db), 'ArrowRight').focusKey).toBe(k(schemas));
    expect(navigate(rows, k(db), 'ArrowRight').toggle).toBeUndefined();
  });

  it('left collapses an expanded node, else goes to the parent', () => {
    expect(navigate(rows, k(db), 'ArrowLeft').toggle).toEqual({ key: k(db), expand: false });
    expect(navigate(rows, k(pub), 'ArrowLeft').focusKey).toBe(k(schemas));
    expect(navigate(rows, k(other), 'ArrowLeft').focusKey).toBe(k(other));
  });

  it('does nothing on an empty tree', () => {
    expect(navigate([], null, 'ArrowDown').focusKey).toBeNull();
  });
});

describe('activation and menu', () => {
  it('maps kinds to tabs', () => {
    expect(activationTab('table')).toBe('table');
    expect(activationTab('view')).toBe('table');
    expect(activationTab('materializedView')).toBe('table');
    for (const k of ['function', 'procedure', 'trigger', 'sequence', 'index'] as const) {
      expect(activationTab(k)).toBe('ddl');
    }
    expect(activationTab('schema')).toBeNull();
    expect(activationTab('folder')).toBeNull();
  });

  it('offers only applicable actions per kind', () => {
    expect(nodeActions({ kind: 'table', hasChildren: true })).toEqual([
      'openData',
      'viewDdl',
      'copyName',
      'copyFullName',
      'refresh',
    ]);
    expect(nodeActions({ kind: 'function', hasChildren: false })).toEqual([
      'viewDdl',
      'copyName',
      'copyFullName',
    ]);
    expect(nodeActions({ kind: 'folder', hasChildren: true })).toEqual(['refresh']);
    expect(nodeActions({ kind: 'role', hasChildren: false })).toEqual(['copyName']);
  });

  it('builds qualified names', () => {
    expect(qualifiedName(users.path)).toBe('public.users');
    const mysql = childPath(
      childPath(childPath(ROOT_PATH, 'database', 'shop'), 'folder', 'tables'),
      'table',
      'orders',
    );
    expect(qualifiedName(mysql)).toBe('shop.orders');
    expect(qualifiedName(db.path)).toBe('app');
    expect(
      qualifiedName(childPath(childPath(users.path, 'folder', 'columns'), 'column', 'id')),
    ).toBe('public.users.id');
  });
});
