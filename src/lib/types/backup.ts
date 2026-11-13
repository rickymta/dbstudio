// Backup / restore (dbstudio-backup) and background behavior.

import type { CloseBehavior, ObjectPath, Uuid } from './core';
import type { JobError } from './jobs';
import type { ExportContent } from './export';
import type { DdlOptions } from './model';

/** Passphrase text (never persisted by the frontend). */
export type Secret = string;

export type NativeTool =
  | 'pgDump'
  | 'pgRestore'
  | 'psql'
  | 'mySqlDump'
  | 'mySqlClient'
  | 'mongoDump'
  | 'mongoRestore'
  | 'expdp'
  | 'impdp'
  | 'redisCli'
  | 'msSqlServerBackup';

export type Compression =
  { kind: 'none' } | { kind: 'gzip'; level: number } | { kind: 'zstd'; level: number };

export type BackupEngine =
  | { kind: 'logical'; content?: ExportContent; ddl?: DdlOptions }
  | { kind: 'native'; tool: NativeTool; options?: Record<string, string> };

export type Destination =
  /** Placeholders: {connection} {database} {date:%Y%m%d-%H%M%S} {job}. */
  | { kind: 'localDir'; path: string; fileTemplate?: string }
  /** MSSQL / Oracle native: directory on the SERVER. */
  | { kind: 'serverDir'; path: string };

export interface BackupSpec {
  connectionId: Uuid;
  targets: ObjectPath[];
  engine: BackupEngine;
  compression?: Compression;
  encrypted?: boolean;
  destination: Destination;
}

export type ScheduleTz = 'local' | 'utc';

export interface Schedule {
  /** 5 fields. */
  cron: string;
  timezone: ScheduleTz;
}

export interface Retention {
  keepLast?: number | null;
  maxAgeDays?: number | null;
}

export type MissedRunPolicy = 'runOnce' | 'skip';

export interface NotifyPolicy {
  onSuccess: boolean;
  onFailure: boolean;
}

export interface BackupJob {
  id: Uuid;
  name: string;
  enabled: boolean;
  spec: BackupSpec;
  schedule: Schedule | null;
  retention?: Retention;
  missedRun?: MissedRunPolicy;
  notify?: NotifyPolicy;
  lastRunAt: string | null;
  nextRunAt: string | null;
  createdAt: string;
  updatedAt: string;
}

export type RunKind = 'backup' | 'restore' | 'verify';
export type RunTrigger = 'manual' | 'schedule' | 'catchUp' | 'cli';
export type RunStatus = 'running' | 'succeeded' | 'failed' | 'cancelled' | 'interrupted';

export interface BackupRun {
  id: Uuid;
  jobId: Uuid | null;
  jobName: string;
  connectionId: Uuid | null;
  kind: RunKind;
  trigger: RunTrigger;
  engine: string;
  status: RunStatus;
  startedAt: string;
  finishedAt: string | null;
  durationMs: number | null;
  filePath: string | null;
  sizeBytes: number | null;
  checksum: string | null;
  objects: number | null;
  rows: number | null;
  error: JobError | null;
  logTail: string | null;
}

export interface RestoreSpec {
  file: string;
  targetConnectionId: Uuid;
  targetDatabase?: string | null;
  createDatabase?: boolean;
  /** Drop existing objects first (destructive: needs confirmation on production). */
  clean?: boolean;
  objects?: string[];
  continueOnError?: boolean;
}

export interface BackupInspection {
  header: unknown;
  preamble: unknown;
  encrypted: boolean;
  native: NativeTool | null;
}

export type ToolSource = 'configured' | 'path' | 'knownDir';

export interface NativeToolInfo {
  tool: NativeTool;
  path: string | null;
  version: string | null;
  source: ToolSource;
  problem: string | null;
}

export interface BackupOutcome {
  files: string[];
  sizeBytes: number;
  checksum: string;
  objects: number;
  rows: number;
}

export interface BackgroundSettings {
  closeToTray: CloseBehavior;
  autostart: boolean;
}
