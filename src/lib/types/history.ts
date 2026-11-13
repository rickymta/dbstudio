// Query history, saved queries, editor tab state, ERD layouts (dbstudio-store).

import type { DbKind, ObjectPath, Uuid } from './core';

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
