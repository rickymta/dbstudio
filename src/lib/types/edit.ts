// Data editing: editability and change sets (dbstudio-core `edit.rs`).

import type { ObjectPath, Value } from './core';

export type NotEditableReason = 'noUniqueKey' | 'notATable' | 'readOnlyConnection' | 'unsupported';

export interface Editability {
  editable: boolean;
  keyColumns: string[];
  /** identity / generated / computed / rowversion columns. */
  readOnlyColumns: string[];
  reason: NotEditableReason | null;
  /** `apply_changes` is all-or-nothing (false for Mongo standalone and Redis). */
  atomic: boolean;
}

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

export type DocumentChange =
  | { op: 'insert'; document: DocInput }
  | { op: 'replace'; id: Value; document: DocInput }
  | { op: 'update'; id: Value; set: FieldEdit[]; unset?: string[][] }
  | { op: 'delete'; id: Value };

export interface DocumentChangeSet {
  collection: ObjectPath;
  changes: DocumentChange[];
}

/** Key names use the Redis key-name encoding (see `keyvalue.ts`). */
export type KeyChange =
  | { op: 'setString'; key: string; value: Value; ttlMs?: number | null }
  | { op: 'hashSet'; key: string; entries: [Value, Value][] }
  | { op: 'hashDelete'; key: string; fields: Value[] }
  | { op: 'listSet'; key: string; index: number; value: Value }
  | { op: 'listPush'; key: string; values: Value[]; head: boolean }
  | { op: 'listRemoveAt'; key: string; index: number }
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
  | { op: 'delete'; keys: string[] };

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
