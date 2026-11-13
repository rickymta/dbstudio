import { describe, expect, it } from 'vitest';
import { childPath, ROOT_PATH } from '../objectPath';
import { mockObjectSearch } from './search';

describe('mockObjectSearch', () => {
  it('ranks exact, then prefix, then substring and honours limit', () => {
    const names = mockObjectSearch('postgres', {
      query: 'orders',
      scope: ROOT_PATH,
      limit: 50,
    }).map((n) => n.name);
    expect(names[0]).toBe('orders');
    expect(names).toContain('orders_id_seq');
    expect(names.indexOf('orders')).toBeLessThan(names.indexOf('archive_old_orders'));
    // prefix match (orders_id_seq) before substring match (archive_old_orders)
    expect(names.indexOf('orders_id_seq')).toBeLessThan(names.indexOf('archive_old_orders'));
    expect(
      mockObjectSearch('postgres', { query: 'orders', scope: ROOT_PATH, limit: 2 }),
    ).toHaveLength(2);
  });

  it('returns paths that match the tree, including folder segments', () => {
    const [hit] = mockObjectSearch('postgres', {
      query: 'users',
      scope: ROOT_PATH,
      kinds: ['table'],
      limit: 5,
    });
    expect(hit!.path.segments.map((s) => s.kind)).toEqual([
      'database',
      'folder',
      'schema',
      'folder',
      'table',
    ]);
    expect(hit!.path.segments.at(-1)!.name).toBe('users');
  });

  it('respects scope and kinds, and is case-insensitive', () => {
    const scope = childPath(ROOT_PATH, 'database', 'analytics');
    const inScope = mockObjectSearch('postgres', { query: 'PAGE', scope, limit: 10 });
    expect(inScope.map((n) => n.name)).toEqual(['page_views']);
    expect(mockObjectSearch('postgres', { query: 'users', scope, limit: 10 })).toEqual([]);
    expect(
      mockObjectSearch('postgres', {
        query: 'orders',
        scope: ROOT_PATH,
        kinds: ['function'],
        limit: 10,
      }),
    ).toEqual([]);
    expect(mockObjectSearch('postgres', { query: '  ', scope: ROOT_PATH, limit: 10 })).toEqual([]);
  });
});
