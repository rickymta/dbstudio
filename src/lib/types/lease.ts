// Leases: a dedicated connection per editor tab (dbstudio-core `lease.rs`).

import type { ExecSummary, Uuid } from './core';

export interface LeaseOptions {
  database?: string | null;
  schema?: string | null;
  /** Default true. */
  autoCommit?: boolean;
}

export type IsolationLevel =
  'readUncommitted' | 'readCommitted' | 'repeatableRead' | 'serializable' | 'snapshot';

export interface TxOptions {
  readOnly?: boolean;
  isolation?: IsolationLevel | null;
}

/** `failed` = PostgreSQL "aborted": only ROLLBACK works. */
export type TxStatus = 'idle' | 'active' | 'failed' | 'unknown';

export interface LeaseState {
  autoCommit: boolean;
  transaction: TxStatus;
  /** Query id currently executing. */
  running: Uuid | null;
  /** Query id whose last result set can `lease_fetch_more`. */
  pendingResult: Uuid | null;
  database: string | null;
  schema: string | null;
  /** Connection lost; only `lease_close` is valid. */
  broken: boolean;
}

export interface LeaseContext {
  database?: string | null;
  schema?: string | null;
}

export interface LeaseInfo {
  leaseId: Uuid;
  sessionId: Uuid;
  state: LeaseState;
}

/** Result of `lease_reopen`; the lease keeps its id. */
export interface LeaseReopened {
  lease: LeaseInfo;
  /** A transaction was open when the connection broke and its changes are lost. */
  transactionLost: boolean;
}

export interface LeaseCloseReport {
  rolledBack: boolean;
  forced: boolean;
}

/** Payload of the `lease://state` event. */
export interface LeaseStateEvent {
  leaseId: Uuid;
  state: LeaseState;
}

export interface ExecuteResult {
  summary: ExecSummary;
  /** `null` for `query_execute`. */
  state: LeaseState | null;
  historyId: number | null;
}
