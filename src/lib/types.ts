// Hand-written TypeScript mirror of the Rust IPC types (crates/dbstudio-core, dbstudio-store,
// src-tauri/commands).
// tauri-specta has no stable 2.x release yet; keep these in sync with the Rust definitions.

export type DbKind =
  | 'postgres'
  | 'mysql'
  | 'sqlserver'
  | 'oracle'
  | 'mongodb'
  | 'redis'
  | 'cassandra'
  | 'scylladb'
  | 'clickhouse';

export type DbFamily = 'relational' | 'document' | 'keyValue' | 'wideColumn' | 'columnar';

export interface Capabilities {
  sql: boolean;
  transactions: boolean;
  schemas: boolean;
  multipleDatabases: boolean;
  explain: boolean;
  explainAnalyze: boolean;
  foreignKeys: boolean;
  cancelQuery: boolean;
  logicalBackup: boolean;
  dataEditing: boolean;
  storedRoutines: boolean;
  triggers: boolean;
  views: boolean;
  sequences: boolean;
}

export type DriverStatus = { state: 'available' } | { state: 'planned'; phase: number };

export interface DriverDescriptor {
  kind: DbKind;
  displayName: string;
  defaultPort: number;
  family: DbFamily;
  capabilities: Capabilities;
  status: DriverStatus;
}

export interface AppInfo {
  name: string;
  version: string;
  os: string;
  arch: string;
}

/** Shape of errors serialized from Rust `dbstudio_core::Error`. */
export interface IpcError {
  kind:
    | 'connection'
    | 'authentication'
    | 'query'
    | 'unsupported'
    | 'cancelled'
    | 'timeout'
    | 'tls'
    | 'notFound'
    | 'storage'
    | 'io'
    | 'other';
  message: string;
  position?: number;
  capability?: string;
}

export type Uuid = string;

// ---------------------------------------------------------------------------------------------
// Connections (dbstudio-core config + dbstudio-store model)
// ---------------------------------------------------------------------------------------------

export type TlsMode = 'disable' | 'prefer' | 'require' | 'verifyCa' | 'verifyFull';

export interface TlsConfig {
  mode: TlsMode;
  caCertPath?: string | null;
  clientCertPath?: string | null;
  clientKeyPath?: string | null;
}

export type Environment = 'development' | 'staging' | 'production';

/** Rust `ConnectionConfig`; never contains a password. */
export interface ConnectionConfig {
  id: Uuid;
  name: string;
  kind: DbKind;
  host: string;
  port: number;
  database?: string | null;
  username?: string | null;
  tls: TlsConfig;
  connectTimeoutMs?: number | null;
  readOnly: boolean;
  environment?: Environment | null;
  options: Record<string, string>;
}

/** Rust `ConnectionProfile` (config fields are flattened into the same object). */
export interface ConnectionProfile extends ConnectionConfig {
  groupId?: Uuid | null;
  /** CSS hex color such as `#e5484d`. */
  color?: string | null;
  savePassword: boolean;
  /** A secret is currently stored in the keychain (read-only for the client). */
  hasPassword: boolean;
  sortOrder: number;
  /** ISO-8601. */
  createdAt: string;
  /** ISO-8601. */
  updatedAt: string;
}

export interface ConnectionGroup {
  id: Uuid;
  name: string;
  parentId?: Uuid | null;
  sortOrder: number;
}

export interface ConnectionsSnapshot {
  groups: ConnectionGroup[];
  profiles: ConnectionProfile[];
}

/** What to do with the stored password when saving or testing a profile. */
export type SecretChange =
  { action: 'keep' } | { action: 'set'; value: string } | { action: 'clear' };

export interface ServerInfo {
  product: string;
  version: string;
  extra: Record<string, string>;
}

export interface SessionInfo {
  sessionId: Uuid;
  connectionId: Uuid;
  serverInfo: ServerInfo;
  descriptor: DriverDescriptor;
}

// ---------------------------------------------------------------------------------------------
// Object tree
// ---------------------------------------------------------------------------------------------

export type ObjectKind =
  | 'database'
  | 'schema'
  | 'table'
  | 'view'
  | 'materializedView'
  | 'role'
  | 'column'
  | 'index'
  | 'function'
  | 'procedure'
  | 'trigger'
  | 'sequence'
  | 'collection'
  | 'key'
  | 'folder';

/** Stable keys used as `name` of `folder` path segments; the UI translates them. */
export const FOLDER_KEYS = [
  'tables',
  'views',
  'materializedViews',
  'functions',
  'procedures',
  'triggers',
  'sequences',
  'indexes',
  'columns',
  'roles',
  'schemas',
] as const;
export type FolderKey = (typeof FOLDER_KEYS)[number];

export interface PathSegment {
  kind: ObjectKind;
  name: string;
}

export interface ObjectPath {
  segments: PathSegment[];
}

export interface ObjectNode {
  path: ObjectPath;
  kind: ObjectKind;
  name: string;
  hasChildren: boolean;
  /** Short extra info such as a column type. */
  detail?: string | null;
}

// ---------------------------------------------------------------------------------------------
// Values and query results
// ---------------------------------------------------------------------------------------------

export type ValueKind =
  | 'null'
  | 'bool'
  | 'int'
  | 'uInt'
  | 'float'
  | 'decimal'
  | 'text'
  | 'bytes'
  | 'date'
  | 'time'
  | 'dateTime'
  | 'timestamp'
  | 'uuid'
  | 'json'
  | 'array'
  | 'document';

/**
 * Tagged cell value. `int`/`uInt` are numbers within +-(2^53-1), otherwise decimal strings;
 * `float` may be the strings "NaN", "Infinity", "-Infinity"; `bytes` is base64.
 */
export type Value =
  | { t: 'null' }
  | { t: 'bool'; v: boolean }
  | { t: 'int'; v: number | string }
  | { t: 'uInt'; v: number | string }
  | { t: 'float'; v: number | string }
  | { t: 'decimal'; v: string }
  | { t: 'text'; v: string }
  | { t: 'bytes'; v: string }
  | { t: 'date'; v: string }
  | { t: 'time'; v: string }
  | { t: 'dateTime'; v: string }
  | { t: 'timestamp'; v: string }
  | { t: 'uuid'; v: string }
  | { t: 'json'; v: unknown }
  | { t: 'array'; v: Value[] }
  | { t: 'document'; v: Record<string, Value> };

export interface ColumnMeta {
  name: string;
  typeName: string;
  valueKind: ValueKind;
  nullable?: boolean | null;
}

export type EndOutcome =
  | { kind: 'rows'; rowCount: number; truncated: boolean }
  | { kind: 'affected'; rows: number }
  | { kind: 'done' };

export interface StatementEnd {
  outcome: EndOutcome;
  elapsedMs: number;
}

export interface ExecSummary {
  statements: number;
  elapsedMs: number;
}

/** Event streamed over a Tauri channel while a query or table fetch runs. */
export type QueryEvent =
  | { type: 'columns'; columns: ColumnMeta[] }
  | { type: 'rows'; rows: Value[][] }
  | { type: 'message'; message: string }
  | { type: 'statementEnd'; end: StatementEnd };

// ---------------------------------------------------------------------------------------------
// Table metadata and browsing
// ---------------------------------------------------------------------------------------------

export interface ColumnDetail {
  name: string;
  ordinal: number;
  typeName: string;
  valueKind: ValueKind;
  nullable: boolean;
  default?: string | null;
  comment?: string | null;
  primaryKey: boolean;
  /** auto_increment / identity / generated ... */
  extra?: string | null;
}

export interface IndexDetail {
  name: string;
  columns: string[];
  unique: boolean;
  primary: boolean;
  method?: string | null;
  definition?: string | null;
}

export interface ForeignKeyDetail {
  name: string;
  columns: string[];
  referencedTable: ObjectPath;
  referencedColumns: string[];
  onUpdate?: string | null;
  onDelete?: string | null;
}

export interface TableDetails {
  path: ObjectPath;
  kind: ObjectKind;
  comment?: string | null;
  columns: ColumnDetail[];
  indexes: IndexDetail[];
  foreignKeys: ForeignKeyDetail[];
  estimatedRows?: number | null;
  sizeBytes?: number | null;
}

export const FILTER_OPS = [
  'eq',
  'ne',
  'lt',
  'le',
  'gt',
  'ge',
  'contains',
  'notContains',
  'startsWith',
  'endsWith',
  'isNull',
  'isNotNull',
  'in',
] as const;
export type FilterOp = (typeof FILTER_OPS)[number];

export interface ColumnFilter {
  column: string;
  op: FilterOp;
  values: string[];
}

export interface SortKey {
  column: string;
  descending: boolean;
}

export interface TableDataRequest {
  queryId: Uuid;
  table: ObjectPath;
  filters: ColumnFilter[];
  rawWhere?: string | null;
  sort: SortKey[];
  offset: number;
  limit: number;
  batchSize?: number | null;
}

export interface TableCountRequest {
  table: ObjectPath;
  filters: ColumnFilter[];
  rawWhere?: string | null;
}
