import { describe, expect, it } from 'vitest';
import {
  ROOT_PATH,
  childPath,
  isAncestor,
  lastSegment,
  parentPath,
  pathKey,
  pathsEqual,
} from './objectPath';

describe('objectPath', () => {
  const db = childPath(ROOT_PATH, 'database', 'app');
  const table = childPath(childPath(db, 'folder', 'tables'), 'table', 'users');

  it('builds children without mutating the parent', () => {
    expect(ROOT_PATH.segments).toHaveLength(0);
    expect(db.segments).toEqual([{ kind: 'database', name: 'app' }]);
    expect(lastSegment(table)).toEqual({ kind: 'table', name: 'users' });
    expect(lastSegment(ROOT_PATH)).toBeUndefined();
  });

  it('computes parents and ancestry', () => {
    expect(parentPath(parentPath(table))).toEqual(db);
    expect(parentPath(ROOT_PATH)).toEqual(ROOT_PATH);
    expect(isAncestor(db, table)).toBe(true);
    expect(isAncestor(table, db)).toBe(false);
    expect(isAncestor(db, db)).toBe(false);
  });

  it('compares paths by kind and name', () => {
    expect(pathsEqual(table, childPath(parentPath(table), 'table', 'users'))).toBe(true);
    expect(pathsEqual(table, childPath(parentPath(table), 'view', 'users'))).toBe(false);
    expect(pathsEqual(db, table)).toBe(false);
  });

  it('produces stable unambiguous keys', () => {
    expect(pathKey(table)).toBe(pathKey({ segments: [...table.segments] }));
    const a = childPath(childPath(ROOT_PATH, 'database', 'a/b'), 'table', 'c');
    const b = childPath(
      childPath(childPath(ROOT_PATH, 'database', 'a'), 'table', 'b'),
      'table',
      'c',
    );
    expect(pathKey(a)).not.toBe(pathKey(b));
    expect(pathKey(ROOT_PATH)).toBe('[]');
  });
});
