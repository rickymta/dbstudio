// Fake MongoDB for the browser mock: the object tree plus in-memory collections of non-uniform
// documents (find with a simple filter/sort, count, insert/replace/delete).

import { fromEjson, toEjsonCompact } from '@/features/document/ejson';
import { childPath, lastSegment, ROOT_PATH } from '../objectPath';
import type {
  ApplyResult,
  ColumnMeta,
  DocInput,
  DocumentChange,
  Editability,
  ExecSummary,
  ObjectKind,
  ObjectNode,
  ObjectPath,
  ParsedConnectionUri,
  QueryEvent,
  TableCountRequest,
  TableDataRequest,
  TableDetails,
  Value,
} from '../types';
import { ipcError, sleep } from './util';

const node = (
  parent: ObjectPath,
  kind: ObjectKind,
  name: string,
  hasChildren: boolean,
  detail?: string,
): ObjectNode => ({
  path: childPath(parent, kind, name),
  kind,
  name,
  hasChildren,
  detail: detail ?? null,
});

const COLLECTION_NAMES = ['users', 'orders', 'events'];

/** Children of `path`: databases -> a collections folder -> collections. */
export function mongoChildren(path: ObjectPath): ObjectNode[] {
  const last = lastSegment(path);
  if (!last) return ['admin', 'app'].map((d) => node(ROOT_PATH, 'database', d, true));
  switch (last.kind) {
    case 'database':
      return [node(path, 'folder', 'collections', true)];
    case 'folder':
      return last.name === 'collections'
        ? COLLECTION_NAMES.map((c) =>
            node(path, 'collection', c, false, `~${docsOf(c).length} docs`),
          )
        : [];
    default:
      return [];
  }
}

export function mongoDetails(path: ObjectPath): TableDetails {
  void path;
  throw ipcError('unsupported', 'table details are not available in the browser mock');
}

export function mongoDdl(path: ObjectPath): string {
  return `-- DDL is not available for ${lastSegment(path)?.name ?? ''}`;
}

// ---------------------------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------------------------

type Pairs = [string, Value][];

const txt = (v: string): Value => ({ t: 'text', v });
const int = (v: number): Value => ({ t: 'int', v });
const ext = (type: string, text: string): Value => ({ t: 'ext', v: { type, text } });
const doc = (v: Pairs): Value => ({ t: 'document', v });
const BASE = Date.UTC(2024, 0, 1);

function oid(kind: number, i: number): string {
  return `${(0x64b7f000 + kind).toString(16)}a1b2c3${i.toString(16).padStart(8, '0')}`
    .slice(0, 24)
    .padEnd(24, '0');
}

const CITIES = ['Hanoi', 'Hue', 'Da Nang', 'Saigon', 'Can Tho'];
const NAMES = ['Ada Lovelace', 'Linus Torvalds', 'Grace Hopper', 'Alan Turing', 'Ken Thompson'];
const STATUSES = ['new', 'paid', 'shipped', 'cancelled'];

function makeUsers(): Value[] {
  return Array.from({ length: 30 }, (_, i) => {
    const f: Pairs = [
      ['_id', ext('objectId', oid(1, i))],
      ['name', txt(NAMES[i % NAMES.length]!)],
      ['email', txt(`user${i + 1}@example.com`)],
    ];
    if (i % 5 !== 4) f.push(['age', int(20 + ((i * 7) % 40))]);
    f.push(['score', { t: 'float', v: (i % 10) / 2 }]);
    f.push(['balance', { t: 'decimal', v: (((i * 1234) % 100000) / 100).toFixed(2) }]);
    f.push(['createdAt', { t: 'timestamp', v: new Date(BASE + i * 86_400_000).toISOString() }]);
    if (i % 3 !== 0) {
      f.push([
        'address',
        doc([
          ['city', txt(CITIES[i % CITIES.length]!)],
          ['zip', txt(String(10000 + i))],
        ]),
      ]);
    }
    f.push(['tags', { t: 'array', v: [txt('a'), txt(i % 2 ? 'b' : 'c')] }]);
    if (i % 4 === 0)
      f.push(['loginCount', ext('int64', String(9_007_199_254_740_993n + BigInt(i)))]);
    if (i % 7 === 0) f.push(['vip', { t: 'bool', v: true }]);
    // `nickname: null` (present) versus no key at all.
    if (i % 6 === 1) f.push(['nickname', { t: 'null' }]);
    return doc(f);
  });
}

function makeOrders(): Value[] {
  return Array.from({ length: 40 }, (_, i) => {
    const f: Pairs = [
      ['_id', ext('objectId', oid(2, i))],
      ['userId', ext('objectId', oid(1, i % 30))],
      ['status', txt(STATUSES[i % STATUSES.length]!)],
      ['total', { t: 'decimal', v: (((i * 977) % 50000) / 100).toFixed(2) }],
      ['placedAt', { t: 'timestamp', v: new Date(BASE + i * 3_600_000).toISOString() }],
      [
        'items',
        {
          t: 'array',
          v: Array.from({ length: (i % 3) + 1 }, (_, k) =>
            doc([
              ['sku', txt(`SKU-${i}-${k}`)],
              ['qty', int(k + 1)],
            ]),
          ),
        },
      ],
    ];
    if (i % 4 === 0) f.push(['note', txt('gift wrap')]);
    if (i % 5 === 0)
      f.push([
        'shipping',
        doc([
          ['carrier', txt('DHL')],
          ['tracking', ext('bsonTimestamp', `${1700000000 + i},${i}`)],
        ]),
      ]);
    return doc(f);
  });
}

function makeEvents(): Value[] {
  const kinds = ['login', 'click', 'purchase'];
  return Array.from({ length: 25 }, (_, i) => {
    const kind = kinds[i % kinds.length]!;
    const f: Pairs = [
      ['_id', int(i + 1)],
      ['kind', txt(kind)],
    ];
    if (kind === 'click') f.push(['x', int(i * 3)], ['y', int(i * 2)]);
    if (kind === 'purchase') f.push(['amount', { t: 'decimal', v: String(i) + '.99' }]);
    if (kind === 'login') f.push(['ip', txt(`10.0.0.${i}`)]);
    return doc(f);
  });
}

const store = new Map<string, Value[]>();

function docsOf(name: string): Value[] {
  let d = store.get(name);
  if (!d) {
    d =
      name === 'users'
        ? makeUsers()
        : name === 'orders'
          ? makeOrders()
          : name === 'events'
            ? makeEvents()
            : [];
    store.set(name, d);
  }
  return d;
}

/** Restore the pristine collections (tests). */
export function resetMockMongo(): void {
  store.clear();
}

function collectionName(path: ObjectPath): string {
  const last = lastSegment(path);
  if (!last || last.kind !== 'collection' || !COLLECTION_NAMES.includes(last.name)) {
    throw ipcError('notFound', `collection ${last?.name ?? ''}`);
  }
  return last.name;
}

function pairsOf(v: Value | undefined): Pairs | null {
  return v?.t === 'document' ? v.v : null;
}

function at(v: Value | undefined, path: string[]): Value | undefined {
  let cur = v;
  for (const key of path) {
    if (cur?.t === 'document') cur = cur.v.find(([k]) => k === key)?.[1];
    else if (cur?.t === 'array' && /^\d+$/.test(key)) cur = cur.v[Number(key)];
    else return undefined;
  }
  return cur;
}

function scalar(v: Value | undefined): string | number | boolean | null | undefined {
  if (!v) return undefined;
  switch (v.t) {
    case 'null':
      return null;
    case 'bool':
      return v.v;
    case 'int':
    case 'uInt':
    case 'float':
      return typeof v.v === 'number' ? v.v : Number(v.v);
    case 'decimal':
      return Number(v.v);
    case 'ext':
      return v.v.type === 'int64' ? Number(v.v.text) : v.v.text;
    case 'text':
    case 'timestamp':
    case 'uuid':
    case 'date':
    case 'dateTime':
      return v.v;
    default:
      return JSON.stringify(v);
  }
}

function cmp(a: Value | undefined, b: Value | undefined): number {
  const x = scalar(a);
  const y = scalar(b);
  if (x === y) return 0;
  if (x === undefined) return -1;
  if (y === undefined) return 1;
  if (x === null) return -1;
  if (y === null) return 1;
  return x < y ? -1 : x > y ? 1 : 0;
}

/** Make a shell literal (`{age: {$gt: 3}}`) parseable as JSON. */
function normalizeFilter(text: string): string {
  return text
    .replace(/'([^'\\]*)'/g, '"$1"')
    .replace(/([{,]\s*)([A-Za-z_$][\w$.]*)\s*:/g, '$1"$2":');
}

function matchesOp(value: Value | undefined, op: string, arg: Value): boolean {
  switch (op) {
    case '$eq':
      return cmp(value, arg) === 0 && value !== undefined;
    case '$ne':
      return !(cmp(value, arg) === 0 && value !== undefined);
    case '$gt':
      return value !== undefined && cmp(value, arg) > 0;
    case '$gte':
      return value !== undefined && cmp(value, arg) >= 0;
    case '$lt':
      return value !== undefined && cmp(value, arg) < 0;
    case '$lte':
      return value !== undefined && cmp(value, arg) <= 0;
    case '$in':
      return arg.t === 'array' && arg.v.some((x) => value !== undefined && cmp(value, x) === 0);
    case '$exists':
      return (value !== undefined) === (scalar(arg) !== false && scalar(arg) !== 0);
    default:
      throw ipcError('query', `unknown operator ${op}`);
  }
}

function matchesFilter(d: Value, filter: Pairs): boolean {
  return filter.every(([key, cond]) => {
    const value = at(d, key.split('.'));
    const ops = pairsOf(cond);
    if (ops && ops.length > 0 && ops.every(([k]) => k.startsWith('$'))) {
      return ops.every(([op, arg]) => matchesOp(value, op, arg));
    }
    return value !== undefined && cmp(value, cond) === 0;
  });
}

function parseFilter(raw: string | null | undefined): Pairs {
  if (!raw || raw.trim() === '') return [];
  let parsed: Value;
  try {
    parsed = fromEjson(normalizeFilter(raw));
  } catch (e) {
    throw ipcError('query', `invalid filter: ${(e as Error).message}`);
  }
  const p = pairsOf(parsed);
  if (!p) throw ipcError('query', 'invalid filter: expected a JSON object');
  return p;
}

function select(req: Pick<TableCountRequest, 'table' | 'filters' | 'rawWhere'>): Value[] {
  const filter = parseFilter(req.rawWhere);
  const column = req.filters.map((f): [string, Value] => [f.column, txt(f.values[0] ?? '')]);
  return docsOf(collectionName(req.table)).filter((d) => matchesFilter(d, [...filter, ...column]));
}

export async function mongoCount(req: TableCountRequest): Promise<number> {
  await sleep(20, 60);
  return select(req).length;
}

const COLUMNS: ColumnMeta[] = [{ name: '$document', typeName: 'document', valueKind: 'document' }];

export async function mongoFetch(
  req: TableDataRequest,
  onEvent: (e: QueryEvent) => void,
): Promise<ExecSummary> {
  const started = performance.now();
  await sleep(20, 60);
  const matched = select(req);
  const sorted = [...matched];
  if (req.sort.length > 0) {
    sorted.sort((a, b) => {
      for (const s of req.sort) {
        const path = s.column.split('.');
        const c = cmp(at(a, path), at(b, path));
        if (c !== 0) return s.descending ? -c : c;
      }
      return 0;
    });
  }
  const page = sorted.slice(req.offset, req.offset + req.limit);
  onEvent({ type: 'columns', columns: COLUMNS });
  const batch = Math.max(1, req.batchSize ?? 500);
  for (let i = 0; i < page.length; i += batch) {
    onEvent({ type: 'rows', rows: page.slice(i, i + batch).map((d) => [d]) });
  }
  const elapsedMs = Math.round(performance.now() - started);
  onEvent({
    type: 'statementEnd',
    end: { outcome: { kind: 'rows', rowCount: page.length, truncated: false }, elapsedMs },
  });
  return { statements: 1, elapsedMs };
}

// ---------------------------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------------------------

export function mongoEditability(path: ObjectPath): Editability {
  collectionName(path);
  return { editable: true, keyColumns: ['_id'], readOnlyColumns: [], reason: null, atomic: false };
}

function inputValue(input: DocInput): Value {
  const v = input.kind === 'ejson' ? fromEjson(input.text) : input.value;
  if (v.t !== 'document') throw ipcError('query', 'a document must be a JSON object');
  return v;
}

function sameId(a: Value | undefined, b: Value | undefined): boolean {
  return a !== undefined && b !== undefined && JSON.stringify(a) === JSON.stringify(b);
}

function shell(coll: string, c: DocumentChange): string {
  const e = (v: Value) => toEjsonCompact(v);
  switch (c.op) {
    case 'insert':
      return `db.${coll}.insertOne(${e(inputValue(c.document))})`;
    case 'replace':
      return `db.${coll}.replaceOne({"_id": ${e(c.id)}}, ${e(inputValue(c.document))})`;
    case 'delete':
      return `db.${coll}.deleteOne({"_id": ${e(c.id)}})`;
    case 'update': {
      const set = c.set.map((s) => `${JSON.stringify(s.path.join('.'))}: ${e(s.value)}`);
      const unset = (c.unset ?? []).map((p) => `${JSON.stringify(p.join('.'))}: ""`);
      const parts = [
        set.length ? `"$set": {${set.join(', ')}}` : '',
        unset.length ? `"$unset": {${unset.join(', ')}}` : '',
      ].filter(Boolean);
      return `db.${coll}.updateOne({"_id": ${e(c.id)}}, {${parts.join(', ')}})`;
    }
  }
}

export function mongoPreview(collection: ObjectPath, changes: DocumentChange[]): string[] {
  const name = collectionName(collection);
  return changes.map((c) => shell(name, c));
}

export async function mongoApply(
  collection: ObjectPath,
  changes: DocumentChange[],
): Promise<ApplyResult> {
  await sleep(20, 60);
  const docs = docsOf(collectionName(collection));
  let seq = docs.length;
  const affected: number[] = [];
  changes.forEach((c, index) => {
    const fail = (message: string) =>
      ipcError('changeFailed', message, { index, applied: affected.length });
    const pos = c.op === 'insert' ? -1 : docs.findIndex((d) => sameId(at(d, ['_id']), c.id));
    if (c.op !== 'insert' && pos < 0) {
      affected.push(0);
      return;
    }
    if (c.op === 'insert') {
      let v: Value;
      try {
        v = inputValue(c.document);
      } catch (e) {
        throw fail((e as Error).message);
      }
      const pairs = pairsOf(v)!;
      const withId: Pairs = pairs.some(([k]) => k === '_id')
        ? pairs
        : [['_id', ext('objectId', oid(9, seq++))], ...pairs];
      if (docs.some((d) => sameId(at(d, ['_id']), withId[0]![1]))) throw fail('duplicate key: _id');
      docs.push(doc(withId));
    } else if (c.op === 'replace') {
      const v = pairsOf(inputValue(c.document))!;
      docs[pos] = doc(v.some(([k]) => k === '_id') ? v : [['_id', c.id], ...v]);
    } else if (c.op === 'delete') {
      docs.splice(pos, 1);
    } else {
      const cur = [...pairsOf(docs[pos])!];
      for (const s of c.set) {
        const k = s.path[0]!;
        const i = cur.findIndex(([x]) => x === k);
        if (i >= 0) cur[i] = [k, s.value];
        else cur.push([k, s.value]);
      }
      for (const p of c.unset ?? []) {
        const i = cur.findIndex(([x]) => x === p[0]);
        if (i >= 0) cur.splice(i, 1);
      }
      docs[pos] = doc(cur);
    }
    affected.push(1);
  });
  return { applied: changes.length, affected, atomic: false };
}

// ---------------------------------------------------------------------------------------------
// Connection URI
// ---------------------------------------------------------------------------------------------

/** Minimal `connection_parse_uri` for `mongodb://` / `mongodb+srv://` (the real one is in Rust). */
export async function mockParseMongoUri(uri: string): Promise<ParsedConnectionUri> {
  await sleep(10, 40);
  const m = /^mongodb(\+srv)?:\/\//i.exec(uri.trim());
  if (!m) throw ipcError('other', 'unsupported URI scheme');
  let url: URL;
  try {
    url = new URL(uri.trim().replace(/^mongodb(\+srv)?:/i, 'http:'));
  } catch {
    throw ipcError('other', 'invalid URI');
  }
  const tls = url.searchParams.get('tls') === 'true' || !!m[1];
  const stamp = new Date().toISOString();
  // Like the backend: URI query parameters and the SRV flag become profile options.
  const options: Record<string, string> = {};
  if (m[1]) options.srv = 'true';
  for (const [k, v] of url.searchParams) if (k !== 'tls' && k !== 'ssl') options[k] = v;
  return {
    hasPassword: url.password !== '',
    profile: {
      id: crypto.randomUUID(),
      name: url.hostname,
      kind: 'mongodb',
      host: url.hostname,
      port: url.port ? Number(url.port) : 27017,
      database: url.pathname.replace(/^\//, '') || null,
      username: url.username ? decodeURIComponent(url.username) : null,
      tls: { mode: tls ? 'require' : 'prefer' },
      connectTimeoutMs: null,
      readOnly: false,
      environment: null,
      options,
      groupId: null,
      color: null,
      savePassword: true,
      hasPassword: false,
      sortOrder: 0,
      createdAt: stamp,
      updatedAt: stamp,
    },
  };
}
