// Deterministic fake table data: cell (row, column) is a pure function of the row index, so
// paging, sorting and filtering behave consistently without materializing the rows.

import type {
  ColumnFilter,
  ColumnMeta,
  ExecSummary,
  QueryEvent,
  TableCountRequest,
  TableDataRequest,
  Uuid,
  Value,
} from '../types';
import { lastSegment } from '../objectPath';
import { findRelation, type MockColumn, type MockTable } from './catalog';
import { mongoCount, mongoFetch } from './catalogMongo';
import { mockSessionKind } from './sessions';
import { ipcError, sleep } from './util';

const BASE_TIME = Date.UTC(2024, 0, 1);
const FIRST = ['Ada', 'Linus', 'Grace', 'Alan', 'Edsger', 'Barbara', 'Ken', 'Margaret'];
const LAST = [
  'Lovelace',
  'Torvalds',
  'Hopper',
  'Turing',
  'Dijkstra',
  'Liskov',
  'Thompson',
  'Hamilton',
];
const STATUSES = ['new', 'paid', 'shipped', 'cancelled'];
const KINDS = ['login', 'click', 'purchase', 'error', 'logout', 'view'];

/** Cheap deterministic integer hash. */
function mix(i: number, salt: number): number {
  let h = Math.imul(i + 1, 0x9e3779b1) ^ Math.imul(salt + 1, 0x85ebca6b);
  h = Math.imul(h ^ (h >>> 15), 0x2c1b3c6d);
  h = Math.imul(h ^ (h >>> 12), 0x297a2d39);
  return (h ^ (h >>> 15)) >>> 0;
}

function hex(n: number, len: number): string {
  return n.toString(16).padStart(8, '0').slice(0, len);
}

function bytesBase64(h: number): string {
  return btoa(String.fromCharCode(h & 255, (h >>> 8) & 255, (h >>> 16) & 255, h >>> 24));
}

/** Value of column `col` (index `ci`) at zero-based row `i` of `table`. */
export function cellAt(table: MockTable, col: MockColumn, ci: number, i: number): Value {
  if (col.pk && col.name === 'id') return { t: 'int', v: i + 1 };
  const h = mix(i, ci + 1);
  if (col.nullable && h % (9 + ci) === 0) return { t: 'null' };
  switch (col.valueKind) {
    case 'int':
      if (col.name === 'big_ref') {
        // Beyond 2^53: serialized as a decimal string like the real backend does.
        return { t: 'int', v: (9_007_199_254_740_993n + BigInt(i)).toString() };
      }
      return { t: 'int', v: col.name.endsWith('_id') ? ((i * 7) % 1000) + 1 : h % 10000 };
    case 'bool':
      return { t: 'bool', v: h % 5 !== 0 };
    case 'decimal':
      return { t: 'decimal', v: ((h % 1_000_000) / 100).toFixed(2) };
    case 'timestamp':
      return { t: 'timestamp', v: new Date(BASE_TIME + i * 37_000).toISOString() };
    case 'dateTime':
      return { t: 'dateTime', v: new Date(BASE_TIME + i * 37_000).toISOString().slice(0, 19) };
    case 'date':
      return {
        t: 'date',
        v: new Date(BASE_TIME + (i % 1500) * 86_400_000).toISOString().slice(0, 10),
      };
    case 'uuid':
      return {
        t: 'uuid',
        v: `${hex(mix(i, 11), 8)}-${hex(mix(i, 12), 4)}-4${hex(mix(i, 13), 3)}-8${hex(mix(i, 14), 3)}-${hex(mix(i, 15), 8)}${hex(mix(i, 16), 4)}`,
      };
    case 'json':
      return {
        t: 'json',
        // Text, not a parsed object: the real backend sends the server's JSON text verbatim.
        v: JSON.stringify({
          n: i,
          ok: h % 2 === 0,
          tags: [KINDS[h % KINDS.length], KINDS[(h >>> 4) % KINDS.length]],
        }),
      };
    case 'bytes':
      return { t: 'bytes', v: bytesBase64(h) };
    default:
      return { t: 'text', v: textAt(table, col, i, h) };
  }
}

function textAt(_table: MockTable, col: MockColumn, i: number, h: number): string {
  switch (col.name) {
    case 'email':
      return `user${i + 1}@example.com`;
    case 'full_name':
    case 'name':
      return `${FIRST[h % FIRST.length]} ${LAST[(h >>> 8) % LAST.length]}`;
    case 'status':
      return STATUSES[h % STATUSES.length]!;
    case 'kind':
      return KINDS[h % KINDS.length]!;
    case 'message':
      return `Event ${i + 1} processed in ${h % 900} ms`;
    case 'url':
      return `/products/${h % 500}`;
    case 'sku':
      return `SKU-${String(i + 1).padStart(5, '0')}`;
    case 'ip':
      return `10.${h % 256}.${(h >>> 8) % 256}.${(h >>> 16) % 256}`;
    default:
      return `${col.name}_${i + 1}`;
  }
}

function valueText(v: Value): string | null {
  switch (v.t) {
    case 'null':
      return null;
    case 'json':
      return v.v;
    case 'array':
    case 'document':
      return JSON.stringify(v.v);
    case 'ext':
      return v.v.text;
    default:
      return String(v.v);
  }
}

function compare(a: string, b: string): number {
  const na = Number(a);
  const nb = Number(b);
  if (a.trim() !== '' && b.trim() !== '' && !Number.isNaN(na) && !Number.isNaN(nb)) return na - nb;
  return a < b ? -1 : a > b ? 1 : 0;
}

function matches(text: string | null, f: ColumnFilter): boolean {
  if (f.op === 'isNull') return text === null;
  if (f.op === 'isNotNull') return text !== null;
  if (text === null) return false;
  const v = f.values[0] ?? '';
  const lt = text.toLowerCase();
  const lv = v.toLowerCase();
  switch (f.op) {
    case 'eq':
      return compare(text, v) === 0;
    case 'ne':
      return compare(text, v) !== 0;
    case 'lt':
      return compare(text, v) < 0;
    case 'le':
      return compare(text, v) <= 0;
    case 'gt':
      return compare(text, v) > 0;
    case 'ge':
      return compare(text, v) >= 0;
    case 'contains':
      return lt.includes(lv);
    case 'notContains':
      return !lt.includes(lv);
    case 'startsWith':
      return lt.startsWith(lv);
    case 'endsWith':
      return lt.endsWith(lv);
    case 'in':
      return f.values.some((x) => compare(text, x) === 0);
    default:
      return true;
  }
}

/** Row indexes (ascending) matching `filters`; `null` means "all rows" (no scan needed). */
const matchCache = new Map<string, Uint32Array>();

function matchingRows(
  table: MockTable,
  filters: ColumnFilter[],
  cacheKey: string,
): Uint32Array | null {
  if (filters.length === 0) return null;
  const cached = matchCache.get(cacheKey);
  if (cached) return cached;
  const cols = filters.map((f) => {
    const ci = table.columns.findIndex((c) => c.name === f.column);
    if (ci < 0) throw ipcError('query', `column "${f.column}" does not exist`);
    return ci;
  });
  const out: number[] = [];
  for (let i = 0; i < table.rows; i++) {
    let ok = true;
    for (let k = 0; k < filters.length && ok; k++) {
      const ci = cols[k]!;
      ok = matches(valueText(cellAt(table, table.columns[ci]!, ci, i)), filters[k]!);
    }
    if (ok) out.push(i);
  }
  const arr = Uint32Array.from(out);
  if (matchCache.size >= 4) matchCache.delete(matchCache.keys().next().value!);
  matchCache.set(cacheKey, arr);
  return arr;
}

function resolve(sessionId: Uuid, path: TableCountRequest['table']) {
  const kind = mockSessionKind(sessionId);
  const rel = findRelation(kind, path);
  if (!rel) throw ipcError('notFound', `table ${lastSegment(path)?.name ?? ''}`);
  return rel.table;
}

const key = (table: string, req: TableCountRequest) => JSON.stringify([table, req.filters]);

const running = new Set<Uuid>();
const cancelled = new Set<Uuid>();

/**
 * Count of rows matching the filters (`rawWhere` is ignored by the mock). With a `queryId` the
 * count takes a little while (long enough to cancel) and `queryCancel` rejects it `cancelled`.
 */
export async function tableCount(sessionId: Uuid, req: TableCountRequest): Promise<number> {
  if (mockSessionKind(sessionId) === 'mongodb') return mongoCount(req);
  const id = req.queryId;
  if (id) running.add(id); // registered immediately so an early cancel is not lost
  try {
    await sleep();
    const table = resolve(sessionId, req.table);
    if (id) {
      for (let i = 0; i < 5; i++) {
        if (cancelled.has(id)) throw ipcError('cancelled', 'operation cancelled');
        await new Promise((r) => setTimeout(r, 40));
      }
      if (cancelled.has(id)) throw ipcError('cancelled', 'operation cancelled');
    }
    const rows = matchingRows(table, req.filters, key(lastSegment(req.table)!.name, req));
    return rows ? rows.length : table.rows;
  } finally {
    if (id) {
      running.delete(id);
      cancelled.delete(id);
    }
  }
}

export type FetchMode = 'inline' | 'late';
let fetchMode: FetchMode = 'inline';

/**
 * `late` emulates Tauri: row batches and `statementEnd` are delivered after the fetch promise has
 * resolved (large Channel payloads travel on a separate async IPC round trip).
 */
export function setFetchMode(mode: FetchMode): void {
  fetchMode = mode;
}

/** Ask a running fetch to stop; unknown ids are ignored (the fetch may have just finished). */
export async function queryCancel(sessionId: Uuid, queryId: Uuid): Promise<void> {
  mockSessionKind(sessionId);
  if (running.has(queryId)) cancelled.add(queryId);
  await sleep(10, 40);
}

function yieldToLoop(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 4));
}

/**
 * Stream a page of rows in batches. Sorting is supported on `id` only (other keys are ignored)
 * and `rawWhere` is ignored. Rejects with a `cancelled` error when `queryCancel` is called.
 */
export async function tableFetch(
  sessionId: Uuid,
  req: TableDataRequest,
  onEvent: (e: QueryEvent) => void,
): Promise<ExecSummary> {
  if (mockSessionKind(sessionId) === 'mongodb') return mongoFetch(req, onEvent);
  const started = performance.now();
  const deferred: QueryEvent[] = [];
  const emit = (e: QueryEvent) => {
    if (fetchMode === 'late' && e.type !== 'columns') deferred.push(e);
    else onEvent(e);
  };
  await sleep();
  const table = resolve(sessionId, req.table);
  running.add(req.queryId);
  try {
    const rows = matchingRows(table, req.filters, key(lastSegment(req.table)!.name, req));
    const total = rows ? rows.length : table.rows;
    const descending = req.sort.some((s) => s.column === 'id' && s.descending);
    const batch = Math.max(1, req.batchSize ?? 500);
    const end = Math.min(total, req.offset + req.limit);

    const columns: ColumnMeta[] = table.columns.map((c) => ({
      name: c.name,
      typeName: c.typeName,
      valueKind: c.valueKind,
      nullable: c.nullable,
    }));
    emit({ type: 'columns', columns });

    let sent = 0;
    for (let pos = req.offset; pos < end; pos += batch) {
      if (cancelled.has(req.queryId)) throw ipcError('cancelled', 'operation cancelled');
      const chunk: Value[][] = [];
      for (let p = pos; p < Math.min(end, pos + batch); p++) {
        const ordered = descending ? total - 1 - p : p;
        const i = rows ? rows[ordered]! : ordered;
        chunk.push(table.columns.map((c, ci) => cellAt(table, c, ci, i)));
      }
      emit({ type: 'rows', rows: chunk });
      sent += chunk.length;
      await yieldToLoop();
    }
    if (cancelled.has(req.queryId)) throw ipcError('cancelled', 'operation cancelled');
    const elapsedMs = Math.round(performance.now() - started);
    emit({
      type: 'statementEnd',
      end: { outcome: { kind: 'rows', rowCount: sent, truncated: false }, elapsedMs },
    });
    if (deferred.length > 0) setTimeout(() => deferred.forEach(onEvent), 15);
    return { statements: 1, elapsedMs };
  } finally {
    running.delete(req.queryId);
    cancelled.delete(req.queryId);
  }
}
