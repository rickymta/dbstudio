// Key-value stores (dbstudio-core `keyvalue.rs`) and paged tree requests (`object.rs`).

import type { ObjectNode, ObjectPath, Value } from './core';

export interface ChildrenPageRequest {
  parent: ObjectPath;
  cursor?: string | null;
  /** Clamped to 1..=5000. */
  limit: number;
  /** Glob (`*`, `?`, `[...]`) on the child name. */
  pattern?: string | null;
}

export interface ChildrenPage {
  nodes: ObjectNode[];
  /** `null` = done. */
  nextCursor: string | null;
  approximateTotal: number | null;
}

export interface KeyReadRequest {
  /** `[Database "0", (KeyPrefix...), Key k]`. */
  key: ObjectPath;
  cursor?: string | null;
  /** Clamped to 1..=10000. */
  count: number;
  pattern?: string | null;
  /** Default 1 MiB. */
  maxStringBytes?: number | null;
}

export interface StreamEntry {
  id: string;
  fields: [Value, Value][];
}

export type KeyEntries =
  | { type: 'string'; value: Value; totalBytes: number; truncated: boolean }
  | { type: 'hash'; fields: [Value, Value][] }
  | { type: 'list'; start: number; items: Value[] }
  | { type: 'set'; members: Value[] }
  /** Score as Redis text ("inf" is valid). */
  | { type: 'zSet'; members: [Value, string][] }
  | { type: 'stream'; entries: StreamEntry[]; groups: number }
  | { type: 'json'; text: string }
  | { type: 'unsupported'; typeName: string };

export interface KeyValuePage {
  key: ObjectPath;
  /** string|hash|list|set|zset|stream|ReJSON-RL|... */
  keyType: string;
  /** `null` = persistent. */
  ttlMs: number | null;
  length: number | null;
  memoryBytes: number | null;
  encoding: string | null;
  entries: KeyEntries;
  nextCursor: string | null;
}
