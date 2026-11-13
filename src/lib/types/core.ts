// Hand-written TypeScript mirror of the Rust IPC types (crates/dbstudio-core, dbstudio-store,
// src-tauri/commands).
// tauri-specta has no stable 2.x release yet; keep these in sync with the Rust definitions.
// 64-bit integers outside `Value` (elapsedMs, rows, ids...) are plain `number`s: they stay far
// below 2^53 in practice. Only `Value::Int`/`UInt` can carry a decimal string.

import type { ConfirmationRequest, ScriptDialect, TextRange } from './script';
import type { SshConfig } from './ssh';

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
  /** The driver can search objects by name on the server (`object_search`). */
  objectSearch: boolean;
  /** Dedicated connection per editor tab (`lease_*` commands). */
  leases: boolean;
  /** Manual transactions (begin/commit/rollback, auto-commit off). */
  manualTransactions: boolean;
  /** `:name` / `$1` / `?` / `@p` query parameters. */
  queryParams: boolean;
  /** EXPLAIN ANALYZE of data-modifying statements (run inside a rolled back transaction). */
  explainAnalyzeWrites: boolean;
  /** Collections of documents (MongoDB). */
  documentStore: boolean;
  /** Keys with typed values (Redis). */
  keyValueStore: boolean;
  /** `ddl_generate` is available. */
  ddlGeneration: boolean;
  /** `schema_model` is available (ERD, autocomplete). */
  schemaModel: boolean;
  /** The schema model is inferred by sampling documents. */
  schemaInference: boolean;
  /** Whole-table streaming (export / backup). */
  tableStream: boolean;
  /** Bulk insert (import). */
  bulkInsert: boolean;
  /** Native backup tool integration (pg_dump, mysqldump...). */
  nativeBackup: boolean;
  /** The tree is paged (`object_children_page`). */
  pagedTree: boolean;
  /** The server enforces read-only for the session (otherwise only the app blocks writes). */
  readOnlySession: boolean;
  /** Connections can go through an SSH tunnel. */
  sshTunnel: boolean;
}

/** Every capability off; spread it and turn on what a driver supports. */
export const NO_CAPABILITIES: Capabilities = {
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
  leases: false,
  manualTransactions: false,
  queryParams: false,
  explainAnalyzeWrites: false,
  documentStore: false,
  keyValueStore: false,
  ddlGeneration: false,
  schemaModel: false,
  schemaInference: false,
  tableStream: false,
  bulkInsert: false,
  nativeBackup: false,
  pagedTree: false,
  readOnlySession: false,
  sshTunnel: false,
};

export type UnavailableReason = 'nativeClientMissing' | 'nativeClientTooOld';

export type DriverStatus =
  | { state: 'available' }
  | { state: 'planned'; phase: number }
  | { state: 'unavailable'; reason: UnavailableReason };

export interface DriverDescriptor {
  kind: DbKind;
  displayName: string;
  defaultPort: number;
  family: DbFamily;
  capabilities: Capabilities;
  status: DriverStatus;
}

export interface DriverDiagnostics {
  status: DriverStatus;
  message: string;
  helpUrl?: string | null;
}

/** Result of `connection_parse_uri`; the password stays in the backend (`pendingSecretId`). */
export interface ParsedConnectionUri {
  profile: ConnectionProfile;
  hasPassword: boolean;
  pendingSecretId?: string | null;
}

export interface AppInfo {
  name: string;
  version: string;
  os: string;
  arch: string;
}

/** Shape of errors serialized from Rust `dbstudio_core::Error`. */
export type IpcErrorKind =
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
  | 'other'
  | 'readOnly'
  | 'confirmationRequired'
  | 'busy'
  | 'permission'
  | 'driverUnavailable'
  | 'sshHostKey'
  | 'integrity'
  | 'changeFailed';

export interface IpcError {
  kind: IpcErrorKind;
  message: string;
  /** `query` and `readOnly`: Unicode scalar offset into the statement text. */
  position?: number;
  capability?: string;
  /** `confirmationRequired`: what the user must confirm. */
  confirmation?: ConfirmationRequest;
  /** `sshHostKey`. */
  host?: string;
  port?: number;
  keyType?: string;
  fingerprint?: string;
  changed?: boolean;
  /** `changeFailed`: index of the failing change and how many were applied before it. */
  index?: number;
  applied?: number;
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
  /** SNI / verification name override (set when connecting through an SSH tunnel). */
  serverName?: string | null;
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
  ssh?: SshConfig | null;
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
  dialect: ScriptDialect;
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
  | 'folder'
  | 'keyPrefix'
  | 'synonym'
  | 'package'
  | 'type';

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
  'collections',
  'synonyms',
  'packages',
  'types',
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
  | 'document'
  | 'ext';

/** Database-specific scalar (`type` is a closed key list, see the design doc 1.2). */
export interface ExtValue {
  type: string;
  text: string;
}

/**
 * Tagged cell value. `int`/`uInt` are numbers within +-(2^53-1), otherwise decimal strings;
 * `json` carries the raw JSON text; `float` may be the strings "NaN", "Infinity", "-Infinity"; `bytes` is base64.
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
  /** `v` is the original JSON text from the server (parsed losslessly by the viewer only). */
  | { t: 'json'; v: string }
  | { t: 'array'; v: Value[] }
  /** Nested document; field order is preserved (array of [name, value] pairs). */
  | { t: 'document'; v: [string, Value][] }
  | { t: 'ext'; v: ExtValue };

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

/** Statement text to run (`QueryRequest` in Rust). */
export interface QueryRequest {
  text: string;
  queryId: Uuid;
  /** Maximum rows kept per result set; extra rows set `truncated`. */
  maxRows?: number | null;
  timeoutMs?: number | null;
  batchSize?: number | null;
  database?: string | null;
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
  | { type: 'statementStart'; start: { index: number; range: TextRange } }
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
  /** When set, the count can be cancelled with `queryCancel` (it then fails with kind `cancelled`). */
  queryId?: string;
}

// ---------------------------------------------------------------------------------------------
// Object search and application settings
// ---------------------------------------------------------------------------------------------

export interface ObjectSearchRequest {
  query: string;
  /** Subtree to search; the root path searches the whole server. */
  scope: ObjectPath;
  /** Restrict results to these kinds; all searchable kinds when omitted. */
  kinds?: ObjectKind[];
  limit: number;
}

export type LogLevel = 'error' | 'warn' | 'info' | 'debug' | 'trace';
export type TimestampDisplay = 'utc' | 'local';

export interface AppSettings {
  logLevel: LogLevel;
  gridMaxRows: number;
  gridMaxCells: number;
  timestampDisplay: TimestampDisplay;
  /** Rows kept by the SQL editor per result set. */
  editorMaxRows: number;
  /** 0 = no timeout. */
  queryTimeoutMs: number;
  historyEnabled: boolean;
  historyMaxPerConnection: number;
  /** LOB cells above this size come back as truncated `ext` values. */
  maxLobBytes: number;
  planMisestimateFactor: number;
  closeToTray: CloseBehavior;
}

export type CloseBehavior = 'auto' | 'always' | 'never';

export const DEFAULT_SETTINGS: AppSettings = {
  logLevel: 'info',
  gridMaxRows: 1_000_000,
  gridMaxCells: 20_000_000,
  timestampDisplay: 'utc',
  editorMaxRows: 1000,
  queryTimeoutMs: 0,
  historyEnabled: true,
  historyMaxPerConnection: 5000,
  maxLobBytes: 16 * 1024 * 1024,
  planMisestimateFactor: 10,
  closeToTray: 'auto',
};
