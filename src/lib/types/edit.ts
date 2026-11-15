// Data editing: editability and change sets (dbstudio-core `edit.rs`).

import type { KeyData } from './keyvalue';
import type { ColumnMeta, ObjectPath, Value } from './core';

/** Caveats the UI shows before changes are applied. */
export type EditNote = 'upsertSemantics' | 'conditionalWrites' | 'asyncMutation';

export type NotEditableReason = 'noUniqueKey' | 'notATable' | 'readOnlyConnection' | 'unsupported';

export interface Editability {
  editable: boolean;
  keyColumns: string[];
  /** identity / generated / computed / rowversion columns. */
  readOnlyColumns: string[];
  reason: NotEditableReason | null;
  /** `apply_changes` is all-or-nothing (false for Mongo standalone and Redis). */
  atomic: boolean;
  /** Caveats to show before applying (empty for classic SQL tables). */
  notes?: EditNote[];
}

export type ResultNotEditableReason =
  | 'noOrigin'
  | 'multipleTables'
  | 'noUniqueKey'
  | 'notATable'
  | 'readOnlyConnection'
  | 'keyNotInResult'
  | 'unsupported';

/**
 * Whether the rows of a query result can be edited by key (`result_editability`).
 * `keyColumns` / `readOnlyColumns` are TABLE column names; `columnMap[i]` is the table column
 * of result column `i` (null: expression, duplicate or unknown column, never edited).
 */
export interface ResultEditability {
  editable: boolean;
  table: ObjectPath | null;
  keyColumns: string[];
  columnMap: (string | null)[];
  readOnlyColumns: string[];
  reason: ResultNotEditableReason | null;
  /** Key columns missing from the result (with `keyNotInResult`). */
  missingKeyColumns?: string[];
  atomic: boolean;
  notes?: EditNote[];
}

/** The columns `result_editability` looks at (kept as `ColumnMeta`). */
export type ResultColumns = ColumnMeta[];

export type EditValue =
  { kind: 'value'; v: Value } | { kind: 'text'; v: string } | { kind: 'default' };

export interface CellEdit {
  column: string;
  value: EditValue;
}

export type RowChange =
  | { op: 'update'; key: Value[]; set: CellEdit[] }
  | { op: 'insert'; values: CellEdit[] }
  | { op: 'delete'; key: Value[] };

export interface RowChangeSet {
  table: ObjectPath;
  keyColumns: string[];
  changes: RowChange[];
}

export type DocInput = { kind: 'value'; value: Value } | { kind: 'ejson'; text: string };

export interface FieldEdit {
  path: string[];
  value: Value;
}

/** Value of a field when the UI read the document; `null` = the field was absent. */
export interface FieldOriginal {
  path: string[];
  value: Value | null;
}

/** `expected` makes the write fail when another client changed those fields meanwhile. */
export type DocumentChange =
  | { op: 'insert'; document: DocInput }
  | { op: 'replace'; id: Value; document: DocInput; expected?: FieldOriginal[] }
  | { op: 'update'; id: Value; set: FieldEdit[]; unset?: string[][]; expected?: FieldOriginal[] }
  | { op: 'delete'; id: Value; expected?: FieldOriginal[] };

/** What `put` does when the key exists. */
export type PutMode = 'replace' | 'ifAbsent';

export interface DocumentChangeSet {
  collection: ObjectPath;
  changes: DocumentChange[];
}

/** Key names use the Redis key-name encoding (see `keyvalue.ts`). */
export type KeyChange =
  | { op: 'setString'; key: string; value: Value; ttlMs?: number | null }
  | { op: 'hashSet'; key: string; entries: [Value, Value][] }
  | { op: 'hashDelete'; key: string; fields: Value[] }
  /** `expected`: value read by the UI; a mismatch fails the change (WATCH + LINDEX). */
  | { op: 'listSet'; key: string; index: number; value: Value; expected?: Value | null }
  | { op: 'listPush'; key: string; values: Value[]; head: boolean }
  | { op: 'listRemoveAt'; key: string; index: number; expected?: Value | null }
  | { op: 'setAdd'; key: string; members: Value[] }
  | { op: 'setRemove'; key: string; members: Value[] }
  /** `members` are `[member, score text]`. */
  | { op: 'zAdd'; key: string; members: [Value, string][] }
  | { op: 'zRemove'; key: string; members: Value[] }
  | { op: 'streamAdd'; key: string; id?: string | null; fields: [Value, Value][] }
  | { op: 'streamDelete'; key: string; ids: string[] }
  /** `ttlMs: null` = PERSIST. */
  | { op: 'expire'; key: string; ttlMs: number | null }
  | { op: 'rename'; key: string; newKey: string }
  | { op: 'delete'; keys: string[] }
  /** Write a whole key (import / restore): temp key then RENAME. */
  | { op: 'put'; key: string; data: KeyData; ttlMs?: number | null; mode?: PutMode };

export interface KeyChangeSet {
  database: ObjectPath;
  changes: KeyChange[];
}

export type ChangeSet =
  | ({ model: 'rows' } & RowChangeSet)
  | ({ model: 'documents' } & DocumentChangeSet)
  | ({ model: 'keys' } & KeyChangeSet);

export interface ApplyResult {
  applied: number;
  affected: number[];
  atomic: boolean;
}
