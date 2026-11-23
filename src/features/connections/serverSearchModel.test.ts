import { describe, expect, it } from 'vitest';
import { childPath, ROOT_PATH } from '@/lib/objectPath';
import type { ObjectKind, ObjectNode } from '@/lib/types';
import {
  buildResultTree,
  extraDetail,
  groupResultsByKind,
  kindGroupLabelKey,
  resultContext,
} from './serverSearchModel';

const pgPath = childPath(
  childPath(
    childPath(
      childPath(childPath(ROOT_PATH, 'database', 'db'), 'folder', 'schemas'),
      'schema',
      'bench',
    ),
    'folder',
    'tables',
  ),
  'table',
  'events',
);

describe('result context and detail', () => {
  it('builds the context from the path without folders or the object', () => {
    expect(resultContext({ path: pgPath })).toBe('db › bench');
  });

  it('drops detail that only repeats the context', () => {
    expect(extraDetail({ path: pgPath, detail: 'db.bench' })).toBe('');
    expect(extraDetail({ path: pgPath, detail: 'bench' })).toBe('');
    expect(extraDetail({ path: pgPath, detail: null })).toBe('');
    const mysql = childPath(
      childPath(childPath(ROOT_PATH, 'database', 'shop'), 'folder', 'tables'),
      'table',
      'orders',
    );
    expect(extraDetail({ path: mysql, detail: 'shop' })).toBe('');
  });

  it('keeps information the context does not have', () => {
    expect(extraDetail({ path: pgPath, detail: 'db.bench, partitioned' })).toBe('partitioned');
    expect(extraDetail({ path: pgPath, detail: 'bench partition' })).toBe('partition');
    expect(extraDetail({ path: pgPath, detail: '~1M rows' })).toBe('~1M rows');
    // A prefix that is not a whole word is not context.
    expect(extraDetail({ path: pgPath, detail: 'benchmark' })).toBe('benchmark');
  });
});

describe('groupResultsByKind', () => {
  const node = (kind: ObjectKind, name: string): ObjectNode => ({
    path: childPath(ROOT_PATH, kind, name),
    kind,
    name,
    hasChildren: false,
    detail: null,
  });

  it('splits the results by kind in a fixed order, keeping the order inside a kind', () => {
    // As the server sends them: best match first, kinds mixed.
    const groups = groupResultsByKind([
      node('procedure', 'sp_User_Reset'),
      node('table', 'Users'),
      node('function', 'GetUserID'),
      node('table', 'VTT_User'),
      node('view', 'v_user'),
      node('procedure', 'sp_User_Insert'),
    ]);
    expect(groups.map((g) => [g.kind, g.nodes.map((n) => n.name)])).toEqual([
      ['table', ['Users', 'VTT_User']],
      ['view', ['v_user']],
      ['function', ['GetUserID']],
      ['procedure', ['sp_User_Reset', 'sp_User_Insert']],
    ]);
  });

  it('has no group for a kind without results and puts unknown kinds last', () => {
    expect(groupResultsByKind([])).toEqual([]);
    const groups = groupResultsByKind([node('role', 'admin'), node('sequence', 'user_seq')]);
    expect(groups.map((g) => g.kind)).toEqual(['sequence', 'role']);
  });

  it('names a group like its folder in the tree', () => {
    expect(kindGroupLabelKey('procedure')).toBe('explorer:folder.procedures');
    expect(kindGroupLabelKey('materializedView')).toBe('explorer:folder.materializedViews');
    expect(kindGroupLabelKey('role')).toBe('explorer:kind.role');
  });
});

describe('buildResultTree', () => {
  const db = (name: string) => childPath(ROOT_PATH, 'database', name);
  const object = (
    database: string,
    schema: string,
    folder: string,
    kind: ObjectKind,
    name: string,
  ): ObjectNode => ({
    path: childPath(
      childPath(
        childPath(childPath(db(database), 'folder', 'schemas'), 'schema', schema),
        'folder',
        folder,
      ),
      kind,
      name,
    ),
    kind,
    name,
    hasChildren: false,
    detail: null,
  });
  /** `name (count)` of every node, indented by depth. */
  const outline = (nodes: ReturnType<typeof buildResultTree>, depth = 0): string[] =>
    nodes.flatMap((n) => [
      `${'  '.repeat(depth)}${n.kind} ${n.name} (${n.count})${n.match ? ' *' : ''}`,
      ...outline(n.children, depth + 1),
    ]);

  it('lays the results out like the object tree, folders in the order of the tree', () => {
    const tree = buildResultTree([
      object('Shop', 'dbo', 'procedures', 'procedure', 'sp_User_Reset'),
      object('Shop', 'dbo', 'tables', 'table', 'Users'),
      object('Report', 'dbo', 'tables', 'table', 'Users'),
      object('Shop', 'dbo', 'functions', 'function', 'GetUserID'),
      object('Shop', 'dbo', 'tables', 'table', 'VTT_User'),
      object('Shop', 'audit', 'tables', 'table', 'user_log'),
    ]);
    expect(outline(tree)).toEqual([
      'database Report (1)',
      '  folder schemas (1)',
      '    schema dbo (1)',
      '      folder tables (1)',
      '        table Users (1) *',
      'database Shop (5)',
      '  folder schemas (5)',
      '    schema audit (1)',
      '      folder tables (1)',
      '        table user_log (1) *',
      '    schema dbo (4)',
      '      folder tables (2)',
      '        table Users (1) *',
      '        table VTT_User (1) *',
      '      folder functions (1)',
      '        function GetUserID (1) *',
      '      folder procedures (1)',
      '        procedure sp_User_Reset (1) *',
    ]);
    expect(tree[1]!.children[0]!.children[1]!.children[0]!.children[0]!.match?.name).toBe('Users');
  });

  it('keeps every database of the tree, empty ones included, but no other empty level', () => {
    const listed = ['master', 'Shop', 'tempdb'].map((name) => ({
      kind: 'database' as const,
      name,
      path: db(name),
    }));
    const roles = {
      kind: 'folder' as const,
      name: 'roles',
      path: childPath(ROOT_PATH, 'folder', 'roles'),
    };
    const tree = buildResultTree(
      [object('Shop', 'dbo', 'tables', 'table', 'Users')],
      [...listed, roles],
    );
    expect(tree.map((n) => `${n.name} (${n.count})`)).toEqual([
      'master (0)',
      'Shop (1)',
      'tempdb (0)',
    ]);
    expect(tree[0]!.children).toEqual([]);
    // Views, functions... of `Shop.dbo` had no match: no empty folder for them.
    expect(outline(tree).filter((line) => line.includes('folder'))).toEqual([
      '  folder schemas (1)',
      '      folder tables (1)',
    ]);
  });

  it('adds no database to results that have no database level', () => {
    const flat: ObjectNode = {
      path: childPath(childPath(ROOT_PATH, 'folder', 'tables'), 'table', 't'),
      kind: 'table',
      name: 't',
      hasChildren: false,
      detail: null,
    };
    const tree = buildResultTree([flat], [{ kind: 'database', name: 'x', path: db('x') }]);
    expect(tree.map((n) => n.name)).toEqual(['tables']);
    expect(buildResultTree([])).toEqual([]);
  });
});
