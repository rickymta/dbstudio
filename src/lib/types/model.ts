// Schema model, DDL generation, streaming and bulk insert (dbstudio-core `model.rs`).

import type { ColumnDetail, ColumnFilter, ObjectKind, ObjectPath, Uuid } from './core';
import type { EditValue } from './edit';

export type ModelDetail = 'keys' | 'full';

export interface SchemaModelRequest {
  /** Database or Schema node. */
  scope: ObjectPath;
  /** Empty = all tables in scope. */
  tables?: ObjectPath[];
  includeViews?: boolean;
  includeComments?: boolean;
  detail?: ModelDetail;
  /** Mongo inference sample size (default 1000). */
  sampleSize?: number | null;
}

export interface TableModel {
  path: ObjectPath;
  kind: ObjectKind;
  schema: string | null;
  name: string;
  comment: string | null;
  columns: ColumnDetail[];
  primaryKey: string[];
  uniqueKeys: string[][];
  estimatedRows: number | null;
}

export interface Relation {
  name: string;
  fromTable: ObjectPath;
  fromColumns: string[];
  toTable: ObjectPath;
  toColumns: string[];
  onUpdate: string | null;
  onDelete: string | null;
  /** Any FK column nullable: "zero or one". */
  fromOptional: boolean;
  oneToOne: boolean;
  /** Mongo heuristic (fooId -> foos._id). */
  inferred: boolean;
}

export interface SchemaModel {
  scope: ObjectPath;
  tables: TableModel[];
  relations: Relation[];
  /** Mongo sampling. */
  inferred: boolean;
  /** More than 2000 tables: first 2000 by name. */
  truncated: boolean;
}

export interface DdlOptions {
  includeDrop?: boolean;
  ifNotExists?: boolean;
  includeIndexes?: boolean;
  includeForeignKeys?: boolean;
  includeTriggers?: boolean;
  includeViews?: boolean;
  includeRoutines?: boolean;
  includeSequences?: boolean;
  includeComments?: boolean;
  includeGrants?: boolean;
  qualifySchema?: boolean;
}

export interface DdlRequest {
  scope: ObjectPath;
  objects?: ObjectPath[];
  options?: DdlOptions;
}

export type DdlSection = 'preData' | 'postData';
export type DdlLanguage = 'sql' | 'mongoShell' | 'cql';

export interface DdlItem {
  path: ObjectPath;
  kind: ObjectKind;
  section: DdlSection;
  sql: string;
  language: DdlLanguage;
  dependsOn: ObjectPath[];
}

export type OnConflict = 'error' | 'skip' | 'update';

export interface TableStreamRequest {
  queryId: Uuid;
  table: ObjectPath;
  columns?: string[];
  filters?: ColumnFilter[];
  rawWhere?: string | null;
  orderByKey?: boolean;
  batchSize?: number | null;
}

export interface BulkInsertRequest {
  table: ObjectPath;
  columns: string[];
  rows: EditValue[][];
  onConflict: OnConflict;
  conflictColumns?: string[];
}

export interface BulkInsertResult {
  inserted: number;
  updated: number;
  skipped: number;
}
