// Query history, saved queries, editor tab state, ERD layouts (dbstudio-store).

import type { DbKind, ObjectPath, Uuid } from './core';
import type { TabDebugState } from './debug';
import type { PlanFormat, RawPlan } from './plan';

export type HistoryStatus = 'running' | 'ok' | 'error' | 'cancelled';
export type HistoryKind = 'query' | 'explain' | 'script' | 'changes';

export interface HistoryEntry {
  id: number;
  connectionId: Uuid;
  database: string | null;
  /** `HistoryKind` (kept as string like the Rust model). */
  kind: string;
  text: string;
  textTruncated: boolean;
  startedAt: string;
  elapsedMs: number | null;
  /** `HistoryStatus`. */
  status: string;
  statements: number | null;
  rows: number | null;
  errorMessage: string | null;
}

export interface HistoryQuery {
  connectionId?: Uuid | null;
  search?: string | null;
  status?: string | null;
  beforeId?: number | null;
  /** Clamped to 1..=500. */
  limit: number;
}

export interface SavedFolder {
  id: Uuid;
  name: string;
  parentId: Uuid | null;
  sortOrder: number;
}

export interface SavedQuery {
  id: Uuid;
  folderId: Uuid | null;
  name: string;
  text: string;
  /** Snippet placeholders `${1:name}`. */
  isSnippet: boolean;
  dbKind: DbKind | null;
  connectionId: Uuid | null;
  description: string | null;
  sortOrder: number;
  createdAt: string;
  updatedAt: string;
}

export interface SavedQueriesSnapshot {
  folders: SavedFolder[];
  queries: SavedQuery[];
}

export interface EditorTabState {
  id: Uuid;
  position: number;
  title: string;
  connectionId: Uuid | null;
  database: string | null;
  schema: string | null;
  text: string;
  filePath: string | null;
  cursor: number;
  autoCommit: boolean;
  /** Text differs from `filePath` on disk (or the tab was never saved). Optional on input. */
  dirty?: boolean;
  /** SQL debugger breakpoints and watches of the tab (`null` = nothing saved). */
  debug?: TabDebugState | null;
}

/** One plan history row without the raw plan (`plan_history_list`). */
export interface PlanHistoryEntry {
  id: Uuid;
  connectionId: Uuid | null;
  createdAt: string;
  database: string | null;
  /** Redacted statement. */
  statement: string;
  analyze: boolean;
  format: PlanFormat;
  totalCost: number | null;
  totalMs: number | null;
  warnings: number;
}

/** `plan_history_get`: the entry plus the same content as a `.dbplan.json` file. */
export interface PlanHistoryRecord extends PlanHistoryEntry {
  rawPlan: RawPlan;
}

export interface SqlFile {
  path: string;
  text: string;
}

export interface ErdLayout {
  id: Uuid;
  connectionId: Uuid;
  name: string;
  scope: ObjectPath;
  /** `[{ path, x, y, collapsed, color? }]`. */
  tables: unknown;
  /** `{ columns, showTypes, showComments, notation }`. */
  options: unknown;
  /** `{ x, y, zoom }`. */
  viewport: unknown;
  /** `{ tablePathKey: [[column, typeName], ...] }` for change detection. */
  snapshot: unknown;
  createdAt: string;
  updatedAt: string;
}

export interface ErdLayoutSummary {
  id: Uuid;
  name: string;
  scope: ObjectPath;
  tableCount: number;
  updatedAt: string;
}
