import { describe, expect, it } from 'vitest';
import { ipc, isTauri } from './ipc';

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
    const redis = drivers.find((d) => d.kind === 'redis');
    expect(redis?.capabilities.sql).toBe(false);
    expect((await ipc.appInfo()).name).toBe('DBStudio');
  });
});
