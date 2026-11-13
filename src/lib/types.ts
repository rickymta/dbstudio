// Hand-written TypeScript mirror of the Rust IPC types (crates/dbstudio-core, src-tauri/commands).
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
    | 'io'
    | 'other';
  message: string;
  position?: number;
  capability?: string;
}
