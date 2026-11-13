// The mock schema model comes from the catalog, so the diagram, the object tree and the editor
// autocomplete all see the same tables.
import { beforeEach, describe, expect, it } from 'vitest';
import { loadSessionCatalog } from '@/features/editor/catalogLoader';
import { ipc } from '../ipc';
import type { ObjectPath } from '../types';
import { resetMockConnections } from './connections';

const PG_DEV = '00000000-0000-4000-8000-0000000000b1';
const MYSQL_STAGING = '00000000-0000-4000-8000-0000000000b2';
const scope = (db: string): ObjectPath => ({ segments: [{ kind: 'database', name: db }] });

beforeEach(() => resetMockConnections());

describe('mock schemaModel from the catalog', () => {
  it('lists the tables of every schema with columns, keys and foreign keys', async () => {
    const s = await ipc.sessionOpen(PG_DEV, 'pw');
    const model = await ipc.schemaModel(s.sessionId, {
      scope: scope('app_dev'),
      includeViews: false,
      includeComments: true,
    });
    const names = model.tables.map((t) => `${t.schema}.${t.name}`);
    expect(names).toEqual(
      expect.arrayContaining(['public.users', 'public.orders', 'audit.audit_log']),
    );
    expect(names).not.toContain('public.active_users');
    expect(model.tables.find((t) => t.name === 'users')).toMatchObject({
      primaryKey: ['id'],
      comment: 'Registered accounts',
    });
    expect(model.relations.map((r) => r.name)).toContain('fk_user_id_users');
    const withViews = await ipc.schemaModel(s.sessionId, {
      scope: scope('app_dev'),
      includeViews: true,
      includeComments: false,
    });
    expect(withViews.tables.map((t) => t.name)).toContain('active_users');
  });

  it('matches the object tree', async () => {
    const s = await ipc.sessionOpen(PG_DEV, 'pw');
    const model = await ipc.schemaModel(s.sessionId, {
      scope: scope('app_dev'),
      includeViews: false,
      includeComments: false,
    });
    const schemas = await ipc.objectChildren(s.sessionId, {
      segments: [
        { kind: 'database', name: 'app_dev' },
        { kind: 'folder', name: 'schemas' },
      ],
    });
    const treeTables: string[] = [];
    for (const schema of schemas) {
      const folder = {
        segments: [...schema.path.segments, { kind: 'folder' as const, name: 'tables' }],
      };
      for (const t of await ipc.objectChildren(s.sessionId, folder)) {
        treeTables.push(`${schema.name}.${t.name}`);
        // The model uses the very same path as the tree node.
        expect(
          model.tables.find((m) => m.path.segments.join() === t.path.segments.join()),
        ).toBeDefined();
      }
    }
    expect(model.tables.map((t) => `${t.schema}.${t.name}`).sort()).toEqual(treeTables.sort());
  });

  it('feeds the editor autocomplete catalog with the same tables (MySQL: no schema)', async () => {
    const s = await ipc.sessionOpen(MYSQL_STAGING, 'pw');
    const catalog = await loadSessionCatalog(s.sessionId, 'shop', null);
    const model = await ipc.schemaModel(s.sessionId, {
      scope: scope('shop'),
      includeViews: true,
      includeComments: false,
    });
    expect(model.tables.map((t) => t.name)).toEqual(
      expect.arrayContaining(['customers', 'orders', 'invoices']),
    );
    expect(model.relations.some((r) => r.fromTable.segments.at(-1)!.name === 'orders')).toBe(true);
    // Columns are known up front (no lazy walk of the tree) and match the model.
    const orders = catalog.catalog.tables.find((t) => t.name === 'orders')!;
    expect(orders.columns.map((c) => c.name)).toEqual(
      model.tables.find((t) => t.name === 'orders')!.columns.map((c) => c.name),
    );
    expect(catalog.catalog.tables.map((t) => t.name).sort()).toEqual(
      model.tables.map((t) => t.name).sort(),
    );
  });

  it('answers an empty model for a scope without a database or another engine', async () => {
    const s = await ipc.sessionOpen(PG_DEV, 'pw');
    expect((await ipc.schemaModel(s.sessionId, { scope: { segments: [] } })).tables).toEqual([]);
    expect((await ipc.schemaModel(s.sessionId, { scope: scope('nope') })).tables).toEqual([]);
  });
});
