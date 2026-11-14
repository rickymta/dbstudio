import { describe, expect, it } from 'vitest';
import { childPath, ROOT_PATH } from '@/lib/objectPath';
import { extraDetail, resultContext, searchScopeDatabase } from './serverSearchModel';

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

describe('searchScopeDatabase', () => {
  const session = (kind: string, extra: Record<string, string> = {}) =>
    ({ descriptor: { kind }, serverInfo: { extra } }) as unknown as Parameters<
      typeof searchScopeDatabase
    >[0];

  it('limits PostgreSQL to the database of the session', () => {
    expect(searchScopeDatabase(session('postgres', { database: 'shop' }), 'other')).toBe('shop');
    expect(searchScopeDatabase(session('postgres'), 'fallback')).toBe('fallback');
    expect(searchScopeDatabase(session('postgres'), null)).toBeUndefined();
  });

  it('does not limit drivers that search the whole server', () => {
    expect(searchScopeDatabase(session('mysql', { database: 'shop' }), 'shop')).toBeUndefined();
  });
});
