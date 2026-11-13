import { describe, expect, it } from 'vitest';
import { ipc, isTauri, toIpcError } from './ipc';
import { ROOT_PATH, childPath } from './objectPath';
import type { QueryEvent, SessionInfo, TableDataRequest, Value } from './types';

describe('ipc outside Tauri', () => {
  it('falls back to mock data', async () => {
    expect(isTauri()).toBe(false);
    const drivers = await ipc.listDrivers();
    expect(drivers.map((d) => d.kind)).toEqual([
      'postgres',
      'mysql',
      'sqlserver',
      'oracle',
      'mongodb',
      'redis',
      'cassandra',
      'scylladb',
      'clickhouse',
    ]);
    expect(drivers.filter((d) => d.status.state === 'available').map((d) => d.kind)).toEqual([
      'postgres',
      'mysql',
    ]);
    const redis = drivers.find((d) => d.kind === 'redis');
    expect(redis?.capabilities.sql).toBe(false);
    expect((await ipc.appInfo()).name).toBe('DBStudio');
  });
});

describe('toIpcError', () => {
  it('normalizes shapes', () => {
    expect(toIpcError({ kind: 'query', message: 'x', position: 3 })).toEqual({
      kind: 'query',
      message: 'x',
      position: 3,
    });
    expect(toIpcError({ kind: 'weird', message: 'x' }).kind).toBe('other');
    expect(toIpcError('boom')).toEqual({ kind: 'other', message: 'boom' });
    expect(toIpcError(new Error('e')).message).toBe('e');
  });
});

describe('mock connections', () => {
  it('seeds, saves, duplicates, tests and deletes', async () => {
    const snap = await ipc.connectionsList();
    expect(snap.groups).toHaveLength(1);
    expect(snap.profiles).toHaveLength(3);
    const first = snap.profiles[0]!;
    const copy = await ipc.connectionDuplicate(first.id);
    expect(copy.id).not.toBe(first.id);
    expect(copy.hasPassword).toBe(false);
    const saved = await ipc.connectionSave(
      { ...copy, name: 'renamed' },
      { action: 'set', value: 'pw' },
    );
    expect(saved.hasPassword).toBe(true);
    expect((await ipc.connectionsList()).profiles).toHaveLength(4);
    await expect(
      ipc.connectionTest({ ...copy, host: 'fail.example' }, { action: 'keep' }),
    ).rejects.toMatchObject({
      kind: 'connection',
    });
    expect((await ipc.connectionTest(copy, { action: 'keep' })).product).toBe('PostgreSQL');
    await ipc.connectionDelete(copy.id);
    expect((await ipc.connectionsList()).profiles).toHaveLength(3);
  });
});

async function open(): Promise<SessionInfo> {
  const snap = await ipc.connectionsList();
  return ipc.sessionOpen(snap.profiles[0]!.id);
}

describe('mock server tree', () => {
  it('browses PostgreSQL down to columns', async () => {
    const s = await open();
    const dbs = await ipc.objectChildren(s.sessionId, ROOT_PATH);
    expect(dbs.map((n) => n.name)).toContain('app_dev');
    const db = dbs.find((n) => n.name === 'app_dev')!;
    const [schemasFolder] = await ipc.objectChildren(s.sessionId, db.path);
    expect(schemasFolder).toMatchObject({ kind: 'folder', name: 'schemas' });
    const schemas = await ipc.objectChildren(s.sessionId, schemasFolder!.path);
    const pub = schemas.find((n) => n.name === 'public')!;
    const folders = await ipc.objectChildren(s.sessionId, pub.path);
    expect(folders.map((f) => f.name)).toEqual(
      expect.arrayContaining(['tables', 'views', 'materializedViews', 'functions', 'sequences']),
    );
    const tables = await ipc.objectChildren(s.sessionId, childPath(pub.path, 'folder', 'tables'));
    const users = tables.find((t) => t.name === 'users')!;
    const [cols] = await ipc.objectChildren(s.sessionId, users.path);
    const columns = await ipc.objectChildren(s.sessionId, cols!.path);
    expect(columns.map((c) => c.name)).toContain('email');
    const details = await ipc.tableDetails(s.sessionId, users.path);
    expect(details.columns[0]).toMatchObject({ name: 'id', primaryKey: true });
    expect(await ipc.objectDdl(s.sessionId, users.path)).toContain('CREATE TABLE');
    const orders = tables.find((t) => t.name === 'orders')!;
    expect(
      (
        await ipc.tableDetails(s.sessionId, orders.path)
      ).foreignKeys[0]?.referencedTable.segments.at(-1)?.name,
    ).toBe('users');
    await ipc.sessionClose(s.sessionId);
  });
});

function eventsRequest(
  s: SessionInfo,
  over: Partial<TableDataRequest> = {},
): { req: TableDataRequest; sessionId: string } {
  const path = [
    'database:app_dev',
    'folder:schemas',
    'schema:public',
    'folder:tables',
    'table:events',
  ];
  let p = ROOT_PATH;
  for (const seg of path) {
    const [kind, name] = seg.split(':') as [never, string];
    p = childPath(p, kind, name);
  }
  return {
    sessionId: s.sessionId,
    req: {
      queryId: crypto.randomUUID(),
      table: p,
      filters: [],
      sort: [],
      offset: 0,
      limit: 1200,
      batchSize: 500,
      ...over,
    },
  };
}

describe('mock table data', () => {
  it('streams batches honoring offset, limit and sort', async () => {
    const s = await open();
    const { req, sessionId } = eventsRequest(s, {
      sort: [{ column: 'id', descending: true }],
      offset: 10,
    });
    const events: QueryEvent[] = [];
    const summary = await ipc.tableFetch(sessionId, req, (e) => events.push(e));
    expect(summary.statements).toBe(1);
    expect(events[0]?.type).toBe('columns');
    const batches = events.filter(
      (e): e is Extract<QueryEvent, { type: 'rows' }> => e.type === 'rows',
    );
    expect(batches.map((b) => b.rows.length)).toEqual([500, 500, 200]);
    expect(batches[0]!.rows[0]![0]).toEqual({ t: 'int', v: 1_000_000 - 10 });
    expect(events.at(-1)).toMatchObject({
      type: 'statementEnd',
      end: { outcome: { rowCount: 1200 } },
    });
    const kinds = new Set<string>(batches.flatMap((b) => b.rows.flat()).map((v: Value) => v.t));
    for (const t of [
      'int',
      'decimal',
      'text',
      'bool',
      'timestamp',
      'date',
      'uuid',
      'json',
      'bytes',
      'null',
    ]) {
      expect(kinds.has(t)).toBe(true);
    }
    const big = batches[0]!.rows[0]![1]!;
    expect(big).toMatchObject({ t: 'int' });
    expect(typeof (big as { v: unknown }).v).toBe('string');
  });

  it('counts consistently with filters', async () => {
    const s = await open();
    const { req, sessionId } = eventsRequest(s);
    expect(await ipc.tableCount(sessionId, { table: req.table, filters: [] })).toBe(1_000_000);
    const filters = [{ column: 'id', op: 'eq' as const, values: ['42'] }];
    expect(await ipc.tableCount(sessionId, { table: req.table, filters })).toBe(1);
    const events: QueryEvent[] = [];
    await ipc.tableFetch(sessionId, { ...req, filters }, (e) => events.push(e));
    const rows = events.flatMap((e) => (e.type === 'rows' ? e.rows : []));
    expect(rows).toHaveLength(1);
    expect(rows[0]![0]).toEqual({ t: 'int', v: 42 });
  });

  it('cancels a running fetch', async () => {
    const s = await open();
    const { req, sessionId } = eventsRequest(s, { limit: 100_000, batchSize: 100 });
    let batches = 0;
    const done = ipc.tableFetch(sessionId, req, (e) => {
      if (e.type === 'rows' && ++batches === 2) void ipc.queryCancel(sessionId, req.queryId);
    });
    await expect(done).rejects.toMatchObject({ kind: 'cancelled' });
    expect(batches).toBeLessThan(1000);
  });
});
