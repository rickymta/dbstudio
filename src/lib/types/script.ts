// Script analysis, query parameters, confirmation (dbstudio-core `script.rs`).
// All offsets are Unicode scalar ("char") offsets, not UTF-16: convert with `src/lib/text.ts`.

import type { QueryRequest } from './core';

export type ScriptDialect =
  | { name: 'postgres' }
  | { name: 'mySql'; mariadb: boolean; backslashEscapes: boolean; ansiQuotes?: boolean }
  | { name: 'msSql' }
  | { name: 'oracle' }
  | { name: 'mongoShell' }
  | { name: 'redisCli' }
  | { name: 'cql' }
  | { name: 'clickHouse' }
  | { name: 'opaque' };

/** Template of `script_generate`. */
export type ScriptTemplate = 'select' | 'insert' | 'update' | 'delete';

/** Result of `script_generate`. */
export interface ScriptGenerated {
  text: string;
  /** Dialect the text is written in. */
  dialect: ScriptDialect;
}

export type ScriptDialectName = ScriptDialect['name'];

/** `end` is exclusive. */
export interface TextRange {
  start: number;
  end: number;
}

export type TxControl =
  'begin' | 'commit' | 'rollback' | 'savepoint' | 'release' | 'rollbackToSavepoint';

export type StatementClass =
  | { kind: 'read' }
  | { kind: 'write' }
  | { kind: 'ddl' }
  | { kind: 'transaction'; op: TxControl }
  | { kind: 'session' }
  | { kind: 'procedural' }
  | { kind: 'explain'; analyze: boolean; writes: boolean }
  | { kind: 'admin' }
  | { kind: 'empty' }
  | { kind: 'unknown' };

export type DestructiveReason =
  | 'drop'
  | 'truncate'
  | 'deleteWithoutWhere'
  | 'updateWithoutWhere'
  | 'alterDrop'
  | 'flushDb'
  | 'flushAll'
  | 'deleteByPattern'
  | 'dropCollection'
  | 'dropDatabase'
  | 'deleteManyAll'
  | 'updateManyAll'
  | 'restore'
  | 'importReplace'
  /** An ordinary data-modifying statement listed for confirmation (e.g. EXPLAIN ANALYZE of a write). */
  | 'write'
  /** A destructive keyword found while scanning a statement the classifier did not understand. */
  | 'keywordScan';

export interface StatementInfo {
  index: number;
  range: TextRange;
  class: StatementClass;
  /** Leading verb for display: "SELECT", "ALTER TABLE", "db.users.find", "FLUSHDB". */
  verb: string;
  destructive: DestructiveReason | null;
  /** MySQL/Oracle DDL commits the open transaction implicitly. */
  implicitCommit: boolean;
  targets: string[];
}

export interface ScriptProblem {
  range: TextRange;
  message: string;
}

export type ParamStyle = 'named' | 'dollar' | 'question' | 'at';

export interface ParamRef {
  /** Key used in `ParamValue.name`: ":id", "$1", "?3", "@p". */
  name: string;
  style: ParamStyle;
  ranges: TextRange[];
}

export interface ScriptAnalysis {
  dialect: ScriptDialect;
  statements: StatementInfo[];
  params: ParamRef[];
  problems: ScriptProblem[];
}

export type ParamInput =
  | { kind: 'null' }
  | { kind: 'text'; v: string }
  | { kind: 'number'; v: string }
  | { kind: 'bool'; v: boolean }
  | { kind: 'raw'; v: string };

export interface ParamValue {
  name: string;
  value: ParamInput;
}

export interface DestructiveStatement {
  index: number;
  range: TextRange;
  text: string;
  reason: DestructiveReason;
}

/** Payload of the `confirmationRequired` error. */
export interface ConfirmationRequest {
  token: string;
  /** Text the user must type exactly. */
  confirmText: string;
  statements: DestructiveStatement[];
  expiresInMs: number;
}

export interface ConfirmationGrant {
  token: string;
  typed: string;
}

/** Body of `query_execute` / `lease_execute` (`QueryRequest` is flattened into it). */
export interface ExecuteRequest extends QueryRequest {
  params?: ParamValue[];
  confirm?: ConfirmationGrant | null;
  /** Record in query history (default true). */
  recordHistory?: boolean;
}

export interface CommandHint {
  name: string;
  syntax: string;
  summary: string;
  flags: string[];
}

export interface Vocabulary {
  keywords: string[];
  functions: string[];
  commands: CommandHint[];
}
