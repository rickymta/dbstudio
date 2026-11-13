// In-memory fake servers (a PostgreSQL one and a MySQL one) used when running outside Tauri.
// The object tree layout mirrors what the real drivers return:
//   PostgreSQL: root -> database -> Folder schemas -> schema -> Folder tables|views|... -> object
//   MySQL:      root -> database -> Folder tables|views|... -> object

import { childPath, lastSegment, ROOT_PATH } from '../objectPath';
import type {
  ColumnDetail,
  DbKind,
  FolderKey,
  ObjectKind,
  ObjectNode,
  ObjectPath,
  TableDetails,
  ValueKind,
} from '../types';
import { mssqlChildren, mssqlDdl, mssqlDetails } from './catalogMssql';
import { mongoChildren, mongoDdl, mongoDetails } from './catalogMongo';
import { oracleChildren, oracleDdl, oracleDetails } from './catalogOracle';
import { redisChildren, redisDdl, redisDetails } from './catalogRedis';
import { ipcError } from './util';

/** Kinds served by their own small catalog (the postgres/mysql ones live in this file). */
const OTHER_CATALOGS: Partial<
  Record<
    DbKind,
    {
      children: (path: ObjectPath) => ObjectNode[];
      details: (path: ObjectPath) => TableDetails;
      ddl: (path: ObjectPath) => string;
    }
  >
> = {
  sqlserver: { children: mssqlChildren, details: mssqlDetails, ddl: mssqlDdl },
  oracle: { children: oracleChildren, details: oracleDetails, ddl: oracleDdl },
  mongodb: { children: mongoChildren, details: mongoDetails, ddl: mongoDdl },
  redis: { children: redisChildren, details: redisDetails, ddl: redisDdl },
};

export type RelationKind = 'table' | 'view' | 'materializedView';

export interface MockColumn {
  name: string;
  typeName: string;
  valueKind: ValueKind;
  nullable: boolean;
  pk: boolean;
  default?: string;
  extra?: string;
}

interface MockForeignKey {
  name: string;
  columns: string[];
  refTable: string;
  refColumns: string[];
}

export interface MockTable {
  name: string;
  kind: RelationKind;
  rows: number;
  columns: MockColumn[];
  fks: MockForeignKey[];
  /** Extra (non-primary) indexes as column lists. */
  indexes: string[][];
  comment?: string;
}

interface MockSchema {
  name: string;
  tables: MockTable[];
  functions: string[];
  procedures: string[];
  sequences: string[];
  triggers: string[];
}

interface MockDatabase {
  name: string;
  schemas: MockSchema[];
}

type Dialect = 'postgres' | 'mysql';

const TYPES: Record<Dialect, Record<string, string>> = {
  postgres: {
    int: 'integer',
    bigint: 'bigint',
    decimal: 'numeric(12,2)',
    text: 'text',
    bool: 'boolean',
    timestamp: 'timestamp with time zone',
    date: 'date',
    uuid: 'uuid',
    json: 'jsonb',
    bytes: 'bytea',
  },
  mysql: {
    int: 'int',
    bigint: 'bigint',
    decimal: 'decimal(12,2)',
    text: 'varchar(255)',
    bool: 'tinyint(1)',
    timestamp: 'datetime',
    date: 'date',
    uuid: 'char(36)',
    json: 'json',
    bytes: 'blob',
  },
};

const KIND_OF: Record<string, ValueKind> = {
  int: 'int',
  bigint: 'int',
  decimal: 'decimal',
  text: 'text',
  bool: 'bool',
  timestamp: 'timestamp',
  date: 'date',
  uuid: 'uuid',
  json: 'json',
  bytes: 'bytes',
};

function makeBuilder(d: Dialect) {
  const col = (name: string, type: string, opts: Partial<MockColumn> = {}): MockColumn => ({
    name,
    typeName: TYPES[d][type] ?? type,
    valueKind: d === 'mysql' && type === 'timestamp' ? 'dateTime' : (KIND_OF[type] ?? 'text'),
    nullable: false,
    pk: false,
    ...opts,
  });
  const id = () =>
    col('id', 'bigint', { pk: true, extra: d === 'mysql' ? 'auto_increment' : 'identity' });
  const table = (
    name: string,
    rows: number,
    columns: MockColumn[],
    extra: Partial<MockTable> = {},
  ): MockTable => ({ name, kind: 'table', rows, columns, fks: [], indexes: [], ...extra });
  const fk = (column: string, refTable: string): MockForeignKey => ({
    name: `fk_${column}_${refTable}`,
    columns: [column],
    refTable,
    refColumns: ['id'],
  });
  return { col, id, table, fk };
}

/** Large table exercising every value kind (id is 1-based and dense). */
function eventsTable(d: Dialect): MockTable {
  const { col, id, table } = makeBuilder(d);
  return table(
    'events',
    1_000_000,
    [
      id(),
      col('big_ref', 'bigint'),
      col('user_id', 'int', { nullable: true }),
      col('amount', 'decimal'),
      col('kind', 'text'),
      col('message', 'text', { nullable: true }),
      col('is_error', 'bool', { default: d === 'mysql' ? '0' : 'false' }),
      col('created_at', 'timestamp', { default: d === 'mysql' ? 'CURRENT_TIMESTAMP' : 'now()' }),
      col('event_date', 'date'),
      col('trace_id', 'uuid'),
      col('payload', 'json', { nullable: true }),
      col('blob', 'bytes', { nullable: true }),
    ],
    { indexes: [['created_at'], ['user_id']], comment: 'Synthetic event log (1,000,000 rows)' },
  );
}

function commerceTables(d: Dialect): MockTable[] {
  const { col, id, table, fk } = makeBuilder(d);
  const users = table(
    d === 'mysql' ? 'customers' : 'users',
    1000,
    [
      id(),
      col('email', 'text'),
      col('full_name', 'text'),
      col('is_active', 'bool', { default: d === 'mysql' ? '1' : 'true' }),
      col('created_at', 'timestamp'),
    ],
    { indexes: [['email']], comment: 'Registered accounts' },
  );
  const userTable = users.name;
  const products = table(
    'products',
    250,
    [
      id(),
      col('sku', 'text'),
      col('name', 'text'),
      col('price', 'decimal'),
      col('created_on', 'date'),
      col('attrs', 'json', { nullable: true }),
    ],
    { indexes: [['sku']] },
  );
  const orders = table(
    'orders',
    5000,
    [
      id(),
      col('user_id', 'bigint'),
      col('total', 'decimal'),
      col('status', 'text', { default: "'new'" }),
      col('placed_at', 'timestamp'),
      col('note', 'text', { nullable: true }),
    ],
    { fks: [{ ...fk('user_id', userTable) }], indexes: [['user_id'], ['placed_at']] },
  );
  const items = table(
    'order_items',
    12000,
    [
      id(),
      col('order_id', 'bigint'),
      col('product_id', 'bigint'),
      col('quantity', 'int', { default: '1' }),
      col('unit_price', 'decimal'),
    ],
    { fks: [fk('order_id', 'orders'), fk('product_id', 'products')], indexes: [['order_id']] },
  );
  return [users, products, orders, items];
}

function viewOf(name: string, kind: RelationKind, columns: MockColumn[], rows: number): MockTable {
  return { name, kind, rows, columns, fks: [], indexes: [] };
}

function buildPostgres(): MockDatabase[] {
  const { col, id, table } = makeBuilder('postgres');
  const pub: MockSchema = {
    name: 'public',
    tables: [
      ...commerceTables('postgres'),
      table('categories', 24, [
        id(),
        col('name', 'text'),
        col('parent_id', 'bigint', { nullable: true }),
      ]),
      eventsTable('postgres'),
      viewOf(
        'active_users',
        'view',
        [col('id', 'bigint'), col('email', 'text'), col('full_name', 'text')],
        800,
      ),
      viewOf(
        'daily_stats',
        'materializedView',
        [col('day', 'date'), col('event_count', 'bigint')],
        1500,
      ),
    ],
    functions: ['touch_updated_at', 'slugify'],
    procedures: ['archive_old_orders'],
    sequences: ['users_id_seq', 'orders_id_seq'],
    triggers: [],
  };
  const audit: MockSchema = {
    name: 'audit',
    tables: [
      table('audit_log', 3000, [
        id(),
        col('actor', 'text'),
        col('action', 'text'),
        col('at', 'timestamp'),
        col('data', 'json', { nullable: true }),
      ]),
      table('login_attempts', 800, [
        id(),
        col('email', 'text'),
        col('success', 'bool'),
        col('ip', 'text'),
        col('at', 'timestamp'),
      ]),
    ],
    functions: ['log_change'],
    procedures: [],
    sequences: ['audit_log_id_seq'],
    triggers: [],
  };
  const analytics: MockSchema = {
    name: 'public',
    tables: [
      table('page_views', 20000, [
        id(),
        col('url', 'text'),
        col('visitor', 'uuid'),
        col('viewed_at', 'timestamp'),
        col('duration_ms', 'int'),
      ]),
      table('sessions', 4000, [id(), col('visitor', 'uuid'), col('started_at', 'timestamp')]),
    ],
    functions: [],
    procedures: [],
    sequences: [],
    triggers: [],
  };
  return [
    { name: 'app_dev', schemas: [pub, audit] },
    { name: 'analytics', schemas: [analytics] },
    { name: 'postgres', schemas: [{ ...analytics, tables: [] }] },
  ];
}

function buildMysql(): MockDatabase[] {
  const { col, id, table } = makeBuilder('mysql');
  const shop: MockSchema = {
    name: '',
    tables: [
      ...commerceTables('mysql'),
      table('invoices', 4200, [
        id(),
        col('order_id', 'bigint'),
        col('amount', 'decimal'),
        col('issued_at', 'timestamp'),
        col('paid', 'bool'),
      ]),
      eventsTable('mysql'),
      viewOf(
        'v_active_customers',
        'view',
        [col('id', 'bigint'), col('email', 'text'), col('full_name', 'text')],
        800,
      ),
    ],
    functions: ['calc_total'],
    procedures: ['archive_orders'],
    sequences: [],
    triggers: ['orders_before_insert'],
  };
  const legacy: MockSchema = {
    name: '',
    tables: [
      table('accounts', 300, [id(), col('login', 'text'), col('created_at', 'timestamp')]),
      table('logs', 9000, [id(), col('message', 'text'), col('at', 'timestamp')]),
    ],
    functions: [],
    procedures: [],
    sequences: [],
    triggers: [],
  };
  return [
    { name: 'shop', schemas: [shop] },
    { name: 'legacy', schemas: [legacy] },
  ];
}

const catalogs: Record<Dialect, MockDatabase[]> = {
  postgres: buildPostgres(),
  mysql: buildMysql(),
};

function dialectOf(kind: DbKind): Dialect {
  return kind === 'mysql' ? 'mysql' : 'postgres';
}

interface Ctx {
  db?: MockDatabase;
  schema?: MockSchema;
}

function context(kind: DbKind, path: ObjectPath): Ctx {
  const dbSeg = path.segments.find((s) => s.kind === 'database');
  const db = catalogs[dialectOf(kind)].find((d) => d.name === dbSeg?.name);
  const schemaSeg = path.segments.find((s) => s.kind === 'schema');
  const schema = db
    ? dialectOf(kind) === 'mysql'
      ? db.schemas[0]
      : db.schemas.find((s) => s.name === schemaSeg?.name)
    : undefined;
  return { db, schema };
}

/** Schemas (with their relations) of the database named in `path`; `undefined` when unknown. */
export function mockSchemasOf(
  kind: DbKind,
  path: ObjectPath,
): { database: string; schemas: { name: string | null; tables: MockTable[] }[] } | undefined {
  const { db } = context(kind, path);
  if (!db) return undefined;
  const schemaSeg = path.segments.find((s) => s.kind === 'schema');
  const mysql = dialectOf(kind) === 'mysql';
  const schemas = mysql
    ? db.schemas.slice(0, 1)
    : db.schemas.filter((s) => !schemaSeg || s.name === schemaSeg.name);
  return {
    database: db.name,
    schemas: schemas.map((s) => ({ name: mysql ? null : s.name, tables: s.tables })),
  };
}

/** Folder holding relations of `kind` in the object tree. */
export const relationFolder = (kind: RelationKind): FolderKey => RELATION_FOLDER[kind];

/** Locate the table/view at `path`. */
export function findRelation(
  kind: DbKind,
  path: ObjectPath,
): { table: MockTable; db: MockDatabase; schema: MockSchema } | undefined {
  const last = lastSegment(path);
  if (!last || !['table', 'view', 'materializedView'].includes(last.kind)) return undefined;
  const { db, schema } = context(kind, path);
  const table = schema?.tables.find((t) => t.name === last.name && t.kind === last.kind);
  return db && schema && table ? { table, db, schema } : undefined;
}

function node(
  parent: ObjectPath,
  kind: ObjectKind,
  name: string,
  hasChildren: boolean,
  detail?: string,
): ObjectNode {
  return { path: childPath(parent, kind, name), kind, name, hasChildren, detail: detail ?? null };
}

const folder = (parent: ObjectPath, key: FolderKey): ObjectNode =>
  node(parent, 'folder', key, true);

function formatRows(n: number): string {
  return n >= 1_000_000 ? `~${(n / 1_000_000).toFixed(1)}M rows` : `~${n} rows`;
}

const RELATION_FOLDER: Record<RelationKind, FolderKey> = {
  table: 'tables',
  view: 'views',
  materializedView: 'materializedViews',
};

function schemaFolders(parent: ObjectPath, s: MockSchema, d: Dialect): ObjectNode[] {
  const has = (k: RelationKind) => s.tables.some((t) => t.kind === k);
  const out: ObjectNode[] = [folder(parent, 'tables'), folder(parent, 'views')];
  if (d === 'postgres' && has('materializedView')) out.push(folder(parent, 'materializedViews'));
  if (s.functions.length) out.push(folder(parent, 'functions'));
  if (s.procedures.length) out.push(folder(parent, 'procedures'));
  if (s.sequences.length) out.push(folder(parent, 'sequences'));
  if (s.triggers.length) out.push(folder(parent, 'triggers'));
  return out;
}

/** Children of `path` in the fake object tree of a `kind` server. */
export function mockChildren(kind: DbKind, path: ObjectPath): ObjectNode[] {
  const other = OTHER_CATALOGS[kind];
  if (other) return other.children(path);
  const d = dialectOf(kind);
  const last = lastSegment(path);
  if (!last) {
    const dbs = catalogs[d].map((db) => node(ROOT_PATH, 'database', db.name, true));
    return d === 'postgres' ? [...dbs, folder(ROOT_PATH, 'roles')] : dbs;
  }
  const { db, schema } = context(kind, path);
  switch (last.kind) {
    case 'database':
      if (!db) throw ipcError('notFound', `database ${last.name}`);
      return d === 'postgres' ? [folder(path, 'schemas')] : schemaFolders(path, db.schemas[0]!, d);
    case 'schema':
      return schema ? schemaFolders(path, schema, d) : [];
    case 'folder':
      return folderChildren(kind, path, last.name as FolderKey, db, schema);
    case 'table':
    case 'materializedView':
    case 'view': {
      const rel = findRelation(kind, path);
      if (!rel) throw ipcError('notFound', `${last.kind} ${last.name}`);
      return rel.table.kind === 'view'
        ? [folder(path, 'columns')]
        : [folder(path, 'columns'), folder(path, 'indexes')];
    }
    default:
      return [];
  }
}

function folderChildren(
  kind: DbKind,
  path: ObjectPath,
  key: FolderKey,
  db?: MockDatabase,
  schema?: MockSchema,
): ObjectNode[] {
  const d = dialectOf(kind);
  const parent = { segments: path.segments.slice(0, -1) };
  switch (key) {
    case 'roles':
      return ['postgres', 'app', 'readonly'].map((r) => node(path, 'role', r, false));
    case 'schemas':
      return (db?.schemas ?? []).map((s) => node(path, 'schema', s.name, true));
    case 'tables':
    case 'views':
    case 'materializedViews':
      return (schema?.tables ?? [])
        .filter((t) => RELATION_FOLDER[t.kind] === key)
        .map((t) => node(path, t.kind, t.name, true, formatRows(t.rows)));
    case 'functions':
      return (schema?.functions ?? []).map((n) => node(path, 'function', n, false));
    case 'procedures':
      return (schema?.procedures ?? []).map((n) => node(path, 'procedure', n, false));
    case 'sequences':
      return (schema?.sequences ?? []).map((n) => node(path, 'sequence', n, false));
    case 'triggers':
      return (schema?.triggers ?? []).map((n) => node(path, 'trigger', n, false));
    case 'columns': {
      const rel = findRelation(kind, parent);
      return (rel?.table.columns ?? []).map((c) =>
        node(path, 'column', c.name, false, c.typeName + (c.pk ? ' (PK)' : '')),
      );
    }
    case 'indexes': {
      const rel = findRelation(kind, parent);
      if (!rel) return [];
      return indexDetails(rel.table, d).map((i) =>
        node(path, 'index', i.name, false, i.columns.join(', ')),
      );
    }
    default:
      return [];
  }
}

function indexDetails(t: MockTable, d: Dialect) {
  const pk = t.columns.filter((c) => c.pk).map((c) => c.name);
  const list = [];
  if (pk.length) {
    list.push({
      name: d === 'mysql' ? 'PRIMARY' : `${t.name}_pkey`,
      columns: pk,
      unique: true,
      primary: true,
    });
  }
  for (const cols of t.indexes) {
    list.push({
      name: `idx_${t.name}_${cols.join('_')}`,
      columns: cols,
      unique: false,
      primary: false,
    });
  }
  return list;
}

function qualified(kind: DbKind, rel: NonNullable<ReturnType<typeof findRelation>>): string {
  const q = (s: string) => (kind === 'mysql' ? `\`${s}\`` : `"${s}"`);
  return kind === 'mysql'
    ? `${q(rel.db.name)}.${q(rel.table.name)}`
    : `${q(rel.schema.name)}.${q(rel.table.name)}`;
}

export function mockTableDetails(kind: DbKind, path: ObjectPath): TableDetails {
  const other = OTHER_CATALOGS[kind];
  if (other) return other.details(path);
  const rel = findRelation(kind, path);
  if (!rel) throw ipcError('notFound', `table ${lastSegment(path)?.name ?? ''}`);
  const d = dialectOf(kind);
  const { table, db, schema } = rel;
  const columns: ColumnDetail[] = table.columns.map((c, i) => ({
    name: c.name,
    ordinal: i + 1,
    typeName: c.typeName,
    valueKind: c.valueKind,
    nullable: c.nullable,
    default: c.default ?? null,
    comment: null,
    primaryKey: c.pk,
    extra: c.extra ?? null,
  }));
  const tableFolder = childPath(
    d === 'postgres'
      ? childPath(
          childPath(
            childPath(childPath(ROOT_PATH, 'database', db.name), 'folder', 'schemas'),
            'schema',
            schema.name,
          ),
          'folder',
          'tables',
        )
      : childPath(childPath(ROOT_PATH, 'database', db.name), 'folder', 'tables'),
    'table',
    '',
  );
  const refPath = (name: string): ObjectPath => ({
    segments: [...tableFolder.segments.slice(0, -1), { kind: 'table', name }],
  });
  return {
    path,
    kind: table.kind,
    comment: table.comment ?? null,
    columns,
    indexes:
      table.kind === 'view'
        ? []
        : indexDetails(table, d).map((i) => ({
            ...i,
            method: 'btree',
            definition: `CREATE ${i.unique ? 'UNIQUE ' : ''}INDEX ${i.name} ON ${table.name} (${i.columns.join(', ')})`,
          })),
    foreignKeys: table.fks.map((f) => ({
      name: f.name,
      columns: f.columns,
      referencedTable: refPath(f.refTable),
      referencedColumns: f.refColumns,
      onUpdate: 'NO ACTION',
      onDelete: 'CASCADE',
    })),
    estimatedRows: table.rows,
    sizeBytes: table.rows * 96,
  };
}

export function mockObjectDdl(kind: DbKind, path: ObjectPath): string {
  const other = OTHER_CATALOGS[kind];
  if (other) return other.ddl(path);
  const d = dialectOf(kind);
  const last = lastSegment(path);
  if (!last) throw ipcError('notFound', 'object');
  const q = (s: string) => (d === 'mysql' ? `\`${s}\`` : `"${s}"`);
  const rel = findRelation(kind, path);
  if (rel) {
    const { table } = rel;
    const name = qualified(kind, rel);
    if (table.kind === 'view' || table.kind === 'materializedView') {
      const cols = table.columns.map((c) => q(c.name)).join(', ');
      const kw = table.kind === 'view' ? 'VIEW' : 'MATERIALIZED VIEW';
      return `CREATE ${kw} ${name} AS\nSELECT ${cols}\nFROM ${q('source_table')}\nWHERE ${q('id')} > 0;`;
    }
    const lines = table.columns.map((c) => {
      const parts = [`  ${q(c.name)} ${c.typeName}`];
      if (!c.nullable) parts.push('NOT NULL');
      if (c.default) parts.push(`DEFAULT ${c.default}`);
      if (c.extra) parts.push(d === 'mysql' ? 'AUTO_INCREMENT' : 'GENERATED ALWAYS AS IDENTITY');
      return parts.join(' ');
    });
    const pk = table.columns.filter((c) => c.pk).map((c) => q(c.name));
    if (pk.length) lines.push(`  PRIMARY KEY (${pk.join(', ')})`);
    for (const f of table.fks) {
      lines.push(
        `  CONSTRAINT ${q(f.name)} FOREIGN KEY (${f.columns.map(q).join(', ')}) REFERENCES ${q(f.refTable)} (${f.refColumns.map(q).join(', ')})`,
      );
    }
    const tail = d === 'mysql' ? ' ENGINE=InnoDB DEFAULT CHARSET=utf8mb4' : '';
    return `CREATE TABLE ${name} (\n${lines.join(',\n')}\n)${tail};`;
  }
  switch (last.kind) {
    case 'database':
      return d === 'mysql'
        ? `CREATE DATABASE ${q(last.name)} DEFAULT CHARACTER SET utf8mb4;`
        : `CREATE DATABASE ${q(last.name)} ENCODING 'UTF8';`;
    case 'schema':
      return `CREATE SCHEMA ${q(last.name)};`;
    case 'function':
      return `CREATE FUNCTION ${q(last.name)}() RETURNS integer\nLANGUAGE sql AS $$ SELECT 1 $$;`;
    case 'procedure':
      return `CREATE PROCEDURE ${q(last.name)}()\nBEGIN\n  -- ...\nEND;`;
    case 'trigger':
      return `CREATE TRIGGER ${q(last.name)} BEFORE INSERT ON ${q('orders')}\nFOR EACH ROW SET NEW.placed_at = NOW();`;
    case 'sequence':
      return `CREATE SEQUENCE ${q(last.name)} START 1 INCREMENT 1;`;
    case 'role':
      return `CREATE ROLE ${q(last.name)} LOGIN;`;
    default:
      return `-- DDL is not available for ${last.kind} ${last.name}`;
  }
}
