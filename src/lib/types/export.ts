// Export / import (dbstudio-export) and file dialogs (`files` commands).

import type { ObjectPath, Uuid, Value, ValueKind } from './core';
import type { StatementInfo } from './script';
import type { DdlOptions, OnConflict } from './model';

export type ExportContent = 'schema' | 'data' | 'both';
export type TextEncoding = 'utf8' | 'utf8Bom';
export type LineEnding = 'lf' | 'crlf';
export type JsonLayout = 'array' | 'ndjson';
export type ExtendedJson = 'off' | 'relaxed' | 'canonical';
export type BinaryFormat = 'base64' | 'hex';

export type ExportSource =
  | { kind: 'database'; path: ObjectPath }
  | { kind: 'schema'; path: ObjectPath }
  | { kind: 'tables'; paths: ObjectPath[] }
  | { kind: 'query'; text: string; database?: string | null }
  | { kind: 'keys'; database: ObjectPath; pattern: string };

export interface SqlOptions {
  ddl?: DdlOptions;
  insertBatchRows?: number;
  maxStatementBytes?: number;
  wrapInTransaction?: boolean;
  disableFkChecks?: boolean;
  includeColumnNames?: boolean;
}

export interface XlsxOptions {
  dataDictionary?: boolean;
  header?: boolean;
  freezeHeader?: boolean;
  maxCellChars?: number;
}

export interface CsvOptions {
  delimiter?: string;
  quote?: string;
  quoteAll?: boolean;
  header?: boolean;
  encoding?: TextEncoding;
  lineEnding?: LineEnding;
  nullText?: string;
  /** Prefix text cells that start like a formula with `'` (CSV injection). */
  excelSafe?: boolean;
}

export interface JsonOptions {
  layout?: JsonLayout;
  extendedJson?: ExtendedJson;
  pretty?: boolean;
  fields?: string[];
}

export type ExportFormat =
  | ({ kind: 'sql' } & SqlOptions)
  | ({ kind: 'xlsx' } & XlsxOptions)
  | ({ kind: 'csv' } & CsvOptions)
  | ({ kind: 'json' } & JsonOptions);

export interface ExportDestination {
  /** File, or `.zip` when several files are produced. */
  path: string;
  overwrite?: boolean;
}

export interface ValueFormat {
  binary?: BinaryFormat;
  flattenFields?: string[];
}

export interface ExportSpec {
  connectionId: Uuid;
  source: ExportSource;
  content: ExportContent;
  format: ExportFormat;
  destination: ExportDestination;
  values?: ValueFormat;
}

export type ImportFormat =
  | { kind: 'csv'; delimiter: string; quote: string; header: boolean; encoding: TextEncoding }
  | { kind: 'xlsx'; sheet: string; header: boolean }
  | { kind: 'json'; layout: JsonLayout }
  | { kind: 'sql'; continueOnError: boolean };

export interface NewColumn {
  name: string;
  typeName: string;
  nullable: boolean;
  primaryKey: boolean;
}

export type ImportTarget =
  | { kind: 'existing'; table: ObjectPath }
  | { kind: 'new'; parent: ObjectPath; name: string; columns: NewColumn[] }
  | { kind: 'script'; database?: string | null };

export type Transform = 'none' | 'trim' | 'emptyAsNull' | 'trimEmptyAsNull';

export interface ColumnMapping {
  /** Header name or "#3". */
  source: string;
  target: string;
  transform?: Transform;
}

export type ImportMode = { kind: 'singleTransaction' } | { kind: 'batches'; rows: number };

export interface ImportSpec {
  connectionId: Uuid;
  file: string;
  format: ImportFormat;
  target: ImportTarget;
  mapping?: ColumnMapping[];
  onConflict?: OnConflict | null;
  mode: ImportMode;
  /** 0 = stop at first error. */
  maxErrors?: number;
}

export interface ImportPreviewRequest {
  file: string;
  format: ImportFormat;
  /** Default 50. */
  rows?: number;
}

export interface ImportPreview {
  columns: string[];
  rows: Value[][];
  sheets: string[];
  detectedDelimiter: string | null;
  suggestedKinds: ValueKind[];
  estimatedRows: number | null;
  statementsPreview: StatementInfo[];
}

export interface RowError {
  /** 1-based line / row / statement number in the file. */
  line: number;
  message: string;
}

export interface ExportReport {
  files: string[];
  objects: number;
  rows: number;
  bytes: number;
  warnings: string[];
}

export interface ImportReport {
  rowsRead: number;
  inserted: number;
  updated: number;
  skipped: number;
  errors: RowError[];
}

export interface FileFilter {
  name: string;
  extensions: string[];
}

/** Saved export settings (`export_presets`); `spec` is an `ExportSpec` JSON. */
export interface ExportPreset {
  id: Uuid;
  name: string;
  connectionId: Uuid | null;
  spec: unknown;
  createdAt: string;
  updatedAt: string;
}
