// Background jobs (dbstudio-core `job.rs`); progress arrives on the `job://progress` event.

import type { Uuid } from './core';

export type JobId = Uuid;
export type JobKind =
  'export' | 'import' | 'sqlScript' | 'backup' | 'restore' | 'verify' | 'keyDelete';
export type JobStatus = 'queued' | 'running' | 'succeeded' | 'failed' | 'cancelled';

export interface JobProgress {
  /** Stable i18n key: "schema" | "data" | "compress" | "encrypt" | "verify" | "restore". */
  phase: string;
  object: string | null;
  objectsDone: number;
  objectsTotal: number | null;
  rowsDone: number;
  rowsTotal: number | null;
  bytesDone: number;
  bytesTotal: number | null;
  message: string | null;
}

export interface JobError {
  kind: string;
  message: string;
}

export interface JobInfo {
  id: JobId;
  kind: JobKind;
  title: string;
  connectionId: Uuid | null;
  status: JobStatus;
  progress: JobProgress;
  createdAt: string;
  startedAt: string | null;
  finishedAt: string | null;
  error: JobError | null;
  /** Kind-specific outcome, e.g. `{ path, sizeBytes, rows, warnings }`. */
  result: unknown;
  cancellable: boolean;
}

export type JobEvent =
  | { type: 'started'; job: JobInfo }
  | { type: 'progress'; jobId: JobId; progress: JobProgress }
  | { type: 'finished'; job: JobInfo };
