import type { AppInfo, Capabilities, DbFamily, DbKind, DriverDescriptor } from '../types';

// Mirrors `DriverRegistry::descriptors()` with the PostgreSQL and MySQL drivers registered
// (phase 1), so the UI works when opened in a plain browser without the Tauri runtime.

const none: Capabilities = {
  sql: false,
  transactions: false,
  schemas: false,
  multipleDatabases: false,
  explain: false,
  explainAnalyze: false,
  foreignKeys: false,
  cancelQuery: false,
  logicalBackup: false,
  dataEditing: false,
  storedRoutines: false,
  triggers: false,
  views: false,
  sequences: false,
  objectSearch: false,
};

const caps = (on: Partial<Capabilities>): Capabilities => ({ ...none, ...on });

const row = (
  kind: DbKind,
  displayName: string,
  defaultPort: number,
  family: DbFamily,
  phase: number | 'available',
  capabilities: Capabilities,
): DriverDescriptor => ({
  kind,
  displayName,
  defaultPort,
  family,
  capabilities,
  status: phase === 'available' ? { state: 'available' } : { state: 'planned', phase },
});

export const mockDrivers: DriverDescriptor[] = [
  row(
    'postgres',
    'PostgreSQL',
    5432,
    'relational',
    'available',
    caps({
      objectSearch: true,
      sql: true,
      transactions: true,
      schemas: true,
      multipleDatabases: true,
      explain: true,
      explainAnalyze: true,
      foreignKeys: true,
      cancelQuery: true,
      logicalBackup: true,
      dataEditing: true,
      storedRoutines: true,
      triggers: true,
      views: true,
      sequences: true,
    }),
  ),
  row(
    'mysql',
    'MySQL',
    3306,
    'relational',
    'available',
    caps({
      objectSearch: true,
      sql: true,
      transactions: true,
      multipleDatabases: true,
      explain: true,
      explainAnalyze: true,
      foreignKeys: true,
      cancelQuery: true,
      logicalBackup: true,
      dataEditing: true,
      storedRoutines: true,
      triggers: true,
      views: true,
    }),
  ),
  row(
    'sqlserver',
    'SQL Server',
    1433,
    'relational',
    3,
    caps({
      sql: true,
      transactions: true,
      schemas: true,
      multipleDatabases: true,
      explain: true,
      explainAnalyze: true,
      foreignKeys: true,
      cancelQuery: true,
      dataEditing: true,
      storedRoutines: true,
      triggers: true,
      views: true,
      sequences: true,
    }),
  ),
  row(
    'oracle',
    'Oracle',
    1521,
    'relational',
    3,
    caps({
      sql: true,
      transactions: true,
      schemas: true,
      explain: true,
      foreignKeys: true,
      cancelQuery: true,
      dataEditing: true,
      storedRoutines: true,
      triggers: true,
      views: true,
      sequences: true,
    }),
  ),
  row(
    'mongodb',
    'MongoDB',
    27017,
    'document',
    4,
    caps({
      transactions: true,
      multipleDatabases: true,
      explain: true,
      explainAnalyze: true,
      cancelQuery: true,
      logicalBackup: true,
      dataEditing: true,
      views: true,
    }),
  ),
  row(
    'redis',
    'Redis',
    6379,
    'keyValue',
    4,
    caps({ transactions: true, multipleDatabases: true, dataEditing: true }),
  ),
  row(
    'cassandra',
    'Cassandra',
    9042,
    'wideColumn',
    10,
    caps({
      sql: true,
      multipleDatabases: true,
      dataEditing: true,
      storedRoutines: true,
      triggers: true,
      views: true,
    }),
  ),
  row(
    'scylladb',
    'ScyllaDB',
    9042,
    'wideColumn',
    10,
    caps({
      sql: true,
      multipleDatabases: true,
      dataEditing: true,
      storedRoutines: true,
      views: true,
    }),
  ),
  row(
    'clickhouse',
    'ClickHouse',
    8123,
    'columnar',
    10,
    caps({
      sql: true,
      multipleDatabases: true,
      explain: true,
      cancelQuery: true,
      dataEditing: true,
      views: true,
    }),
  ),
];

/** Descriptor of the driver for `kind` (mock registry). */
export function mockDriverFor(kind: DbKind): DriverDescriptor {
  const d = mockDrivers.find((x) => x.kind === kind);
  if (!d) throw new Error(`unknown kind ${kind}`);
  return d;
}

export const mockAppInfo: AppInfo = {
  name: 'DBStudio',
  version: '0.1.0',
  os: 'browser',
  arch: 'n/a',
};
