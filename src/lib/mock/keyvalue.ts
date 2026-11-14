// Mock key-value commands: paged tree (a fake SCAN over `catalogRedis` for Redis, offset paging
// over the plain children list for the other kinds), key reads, ChangeSet::Keys and the
// pattern-delete job.

import type {
  ApplyResult,
  ChildrenPage,
  ChildrenPageRequest,
  ConfirmationGrant,
  JobId,
  JobInfo,
  KeyChange,
  KeyChangeSet,
  KeyEntries,
  KeyReadRequest,
  KeyValuePage,
  ObjectPath,
  Uuid,
  Value,
} from '../types';
import { mockChildren } from './catalog';
import {
  globToRegExp,
  redisChildren,
  redisDatabaseOf,
  redisDb,
  redisMatchingKeys,
  redisNodesForKeys,
  redisPrefixOf,
  type MockRedisKey,
} from './catalogRedis';
import { mockEmit } from './events';
import { jobsMock, mockAddJob } from './jobs';
import { mockSessionKind } from './sessions';
import { clone, ipcError, newId } from './util';

function glob(pattern: string): RegExp {
  const escaped = pattern
    .replace(/[.+^${}()|\\]/g, '\\$&')
    .replace(/\*/g, '.*')
    .replace(/\?/g, '.');
  return new RegExp(`^${escaped}$`, 'i');
}

/** Text used to compare/match element values inside the fake server. */
function vtext(v: Value): string {
  if (v.t === 'null') return '';
  return typeof v.v === 'string' ? v.v : String(v.v);
}

const clamp = (n: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, n));

const TYPE_NAME: Record<MockRedisKey['type'], string> = {
  string: 'string',
  hash: 'hash',
  list: 'list',
  set: 'set',
  zset: 'zset',
  stream: 'stream',
  json: 'ReJSON-RL',
};

const ENCODING: Record<MockRedisKey['type'], string> = {
  string: 'embstr',
  hash: 'listpack',
  list: 'quicklist',
  set: 'listpack',
  zset: 'skiplist',
  stream: 'stream',
  json: 'raw',
};

function lengthOf(k: MockRedisKey): number {
  switch (k.type) {
    case 'string':
      return k.value.t === 'bytes' ? atob(k.value.v).length : vtext(k.value).length;
    case 'hash':
      return k.fields.length;
    case 'list':
      return k.items.length;
    case 'set':
    case 'zset':
      return k.members.length;
    case 'stream':
      return k.entries.length;
    case 'json':
      return k.text.length;
  }
}

function keyOf(path: ObjectPath): { db: string; name: string } {
  const last = path.segments[path.segments.length - 1];
  if (!last || last.kind !== 'key') throw ipcError('notFound', 'not a key path');
  return { db: redisDatabaseOf(path), name: last.name };
}

/** Pages of an unknown key are an error like `TYPE none`. */
function existing(db: string, name: string): MockRedisKey {
  const entry = redisDb(db).get(name);
  if (!entry) throw ipcError('notFound', `key ${name} does not exist`);
  return entry;
}

/** Hook for tests/demos: when true, `keysDeletePattern` demands a typed confirmation. */
let requireConfirmation = false;
export function setMockKeyDeleteConfirmation(on: boolean): void {
  requireConfirmation = on;
}

const pendingGrants = new Map<string, string>();

export function readKeyPage(request: KeyReadRequest): KeyValuePage {
  const { db, name } = keyOf(request.key);
  const entry = existing(db, name);
  const count = clamp(request.count, 1, 10000);
  const offset = Number(request.cursor ?? 0) || 0;
  const re = request.pattern ? globToRegExp(request.pattern) : null;
  let entries: KeyEntries;
  let total = 0;
  const slice = <T>(all: T[]): { page: T[]; next: number } => {
    const page = all.slice(offset, offset + count);
    total = all.length;
    return { page, next: offset + page.length };
  };
  let next = 0;
  switch (entry.type) {
    case 'string': {
      const limit = request.maxStringBytes ?? 1048576;
      const full = entry.value;
      const str = full.t === 'text' ? full.v : null;
      const truncated = str !== null && str.length > limit;
      entries = {
        type: 'string',
        value: truncated && str !== null ? { t: 'text', v: str.slice(0, limit) } : clone(full),
        totalBytes: lengthOf(entry),
        truncated,
      };
      total = 0;
      break;
    }
    case 'hash': {
      const all = re ? entry.fields.filter(([f]) => re.test(vtext(f))) : entry.fields;
      const { page, next: n } = slice(all);
      entries = { type: 'hash', fields: clone(page) };
      next = n;
      break;
    }
    case 'list': {
      const { page, next: n } = slice(entry.items);
      entries = { type: 'list', start: offset, items: clone(page) };
      next = n;
      break;
    }
    case 'set': {
      const all = re ? entry.members.filter((m) => re.test(vtext(m))) : entry.members;
      const { page, next: n } = slice(all);
      entries = { type: 'set', members: clone(page) };
      next = n;
      break;
    }
    case 'zset': {
      const sorted = [...entry.members].sort((a, b) => Number(a[1]) - Number(b[1]));
      const all = re ? sorted.filter(([m]) => re.test(vtext(m))) : sorted;
      const { page, next: n } = slice(all);
      entries = { type: 'zSet', members: clone(page) };
      next = n;
      break;
    }
    case 'stream': {
      const { page, next: n } = slice(entry.entries);
      entries = { type: 'stream', entries: clone(page), groups: entry.groups };
      next = n;
      break;
    }
    case 'json':
      entries = { type: 'json', text: entry.text };
      break;
  }
  return {
    key: request.key,
    keyType: TYPE_NAME[entry.type],
    ttlMs: entry.expiresAt === null ? null : Math.max(1, entry.expiresAt - Date.now()),
    length: lengthOf(entry),
    memoryBytes: 56 + lengthOf(entry) * 24,
    encoding: ENCODING[entry.type],
    entries,
    nextCursor: next > 0 && next < total ? String(next) : null,
  };
}

// --- ChangeSet::Keys ---------------------------------------------------------------------------

const quote = (s: string) => (/^[\w.:/-]+$/.test(s) ? s : JSON.stringify(s));
const q = (v: Value) =>
  quote(
    v.t === 'bytes'
      ? `\\x${[...atob(v.v)].map((c) => c.charCodeAt(0).toString(16).padStart(2, '0')).join('\\x')}`
      : vtext(v),
  );
const qk = (k: string) => quote(k.startsWith('\u0000b64:') ? `<binary:${k.slice(5)}>` : k);

/** Redis commands a change maps to (what the real preview prints). */
export function previewKeyChange(c: KeyChange): string {
  switch (c.op) {
    case 'setString':
      return `SET ${qk(c.key)} ${q(c.value)}${c.ttlMs ? ` PX ${c.ttlMs}` : ' KEEPTTL'}`;
    case 'hashSet':
      return `HSET ${qk(c.key)} ${c.entries.map(([f, v]) => `${q(f)} ${q(v)}`).join(' ')}`;
    case 'hashDelete':
      return `HDEL ${qk(c.key)} ${c.fields.map(q).join(' ')}`;
    case 'listSet':
      return `LSET ${qk(c.key)} ${c.index} ${q(c.value)}`;
    case 'listPush':
      return `${c.head ? 'LPUSH' : 'RPUSH'} ${qk(c.key)} ${c.values.map(q).join(' ')}`;
    case 'listRemoveAt':
      return `LSET ${qk(c.key)} ${c.index} "__dbstudio_deleted__"; LREM ${qk(c.key)} 1 "__dbstudio_deleted__"`;
    case 'setAdd':
      return `SADD ${qk(c.key)} ${c.members.map(q).join(' ')}`;
    case 'setRemove':
      return `SREM ${qk(c.key)} ${c.members.map(q).join(' ')}`;
    case 'zAdd':
      return `ZADD ${qk(c.key)} ${c.members.map(([m, s]) => `${s} ${q(m)}`).join(' ')}`;
    case 'zRemove':
      return `ZREM ${qk(c.key)} ${c.members.map(q).join(' ')}`;
    case 'streamAdd':
      return `XADD ${qk(c.key)} ${c.id ?? '*'} ${c.fields.map(([f, v]) => `${q(f)} ${q(v)}`).join(' ')}`;
    case 'streamDelete':
      return `XDEL ${qk(c.key)} ${c.ids.join(' ')}`;
    case 'expire':
      return c.ttlMs === null ? `PERSIST ${qk(c.key)}` : `PEXPIRE ${qk(c.key)} ${c.ttlMs}`;
    case 'rename':
      return `RENAME ${qk(c.key)} ${qk(c.newKey)}`;
    case 'delete':
      return `DEL ${c.keys.map(qk).join(' ')}`;
    case 'put':
      return `-- PUT ${qk(c.key)} (${c.data.type}${c.mode === 'ifAbsent' ? ', if absent' : ''})`;
  }
}

function wrongType(op: string): never {
  throw ipcError(
    'query',
    `WRONGTYPE Operation against a key holding the wrong kind of value (${op})`,
  );
}

/** Apply one change to the fake database; returns the affected element count. */
function applyOne(db: Map<string, MockRedisKey>, c: KeyChange): number {
  const get = <T extends MockRedisKey['type']>(
    key: string,
    type: T,
    create: () => Extract<MockRedisKey, { type: T }>,
  ): Extract<MockRedisKey, { type: T }> => {
    let e = db.get(key);
    if (!e) {
      e = create();
      db.set(key, e);
    }
    if (e.type !== type) wrongType(c.op);
    return e as Extract<MockRedisKey, { type: T }>;
  };
  switch (c.op) {
    case 'setString': {
      const old = db.get(c.key);
      db.set(c.key, {
        type: 'string',
        value: clone(c.value),
        // SET without a TTL keeps the existing one here (KEEPTTL semantics).
        expiresAt: c.ttlMs ? Date.now() + c.ttlMs : (old?.expiresAt ?? null),
      });
      return 1;
    }
    case 'hashSet': {
      const h = get(c.key, 'hash', () => ({ type: 'hash', fields: [], expiresAt: null }));
      let added = 0;
      for (const [f, v] of c.entries) {
        const i = h.fields.findIndex(([x]) => vtext(x) === vtext(f));
        if (i >= 0) h.fields[i] = [clone(f), clone(v)];
        else {
          h.fields.push([clone(f), clone(v)]);
          added++;
        }
      }
      return added;
    }
    case 'hashDelete': {
      const h = get(c.key, 'hash', () => ({ type: 'hash', fields: [], expiresAt: null }));
      const before = h.fields.length;
      const drop = new Set(c.fields.map(vtext));
      h.fields = h.fields.filter(([f]) => !drop.has(vtext(f)));
      if (h.fields.length === 0) db.delete(c.key);
      return before - h.fields.length;
    }
    case 'listSet': {
      const l = get(c.key, 'list', () => ({ type: 'list', items: [], expiresAt: null }));
      if (c.index < 0 || c.index >= l.items.length)
        throw ipcError('query', 'ERR index out of range');
      l.items[c.index] = clone(c.value);
      return 1;
    }
    case 'listPush': {
      const l = get(c.key, 'list', () => ({ type: 'list', items: [], expiresAt: null }));
      for (const v of c.values) {
        if (c.head) l.items.unshift(clone(v));
        else l.items.push(clone(v));
      }
      return c.values.length;
    }
    case 'listRemoveAt': {
      const l = get(c.key, 'list', () => ({ type: 'list', items: [], expiresAt: null }));
      if (c.index < 0 || c.index >= l.items.length)
        throw ipcError('query', 'ERR index out of range');
      l.items.splice(c.index, 1);
      if (l.items.length === 0) db.delete(c.key);
      return 1;
    }
    case 'setAdd': {
      const s = get(c.key, 'set', () => ({ type: 'set', members: [], expiresAt: null }));
      let added = 0;
      for (const m of c.members) {
        if (!s.members.some((x) => vtext(x) === vtext(m))) {
          s.members.push(clone(m));
          added++;
        }
      }
      return added;
    }
    case 'setRemove': {
      const s = get(c.key, 'set', () => ({ type: 'set', members: [], expiresAt: null }));
      const before = s.members.length;
      const drop = new Set(c.members.map(vtext));
      s.members = s.members.filter((m) => !drop.has(vtext(m)));
      if (s.members.length === 0) db.delete(c.key);
      return before - s.members.length;
    }
    case 'zAdd': {
      const z = get(c.key, 'zset', () => ({ type: 'zset', members: [], expiresAt: null }));
      for (const [m, score] of c.members) {
        if (Number.isNaN(Number(score))) throw ipcError('query', 'ERR value is not a valid float');
        const i = z.members.findIndex(([x]) => vtext(x) === vtext(m));
        if (i >= 0) z.members[i] = [clone(m), score];
        else z.members.push([clone(m), score]);
      }
      return c.members.length;
    }
    case 'zRemove': {
      const z = get(c.key, 'zset', () => ({ type: 'zset', members: [], expiresAt: null }));
      const before = z.members.length;
      const drop = new Set(c.members.map(vtext));
      z.members = z.members.filter(([m]) => !drop.has(vtext(m)));
      if (z.members.length === 0) db.delete(c.key);
      return before - z.members.length;
    }
    case 'streamAdd': {
      const s = get(c.key, 'stream', () => ({
        type: 'stream',
        entries: [],
        groups: 0,
        expiresAt: null,
      }));
      s.entries.push({ id: c.id ?? `${Date.now()}-0`, fields: clone(c.fields) });
      return 1;
    }
    case 'streamDelete': {
      const s = get(c.key, 'stream', () => ({
        type: 'stream',
        entries: [],
        groups: 0,
        expiresAt: null,
      }));
      const before = s.entries.length;
      s.entries = s.entries.filter((e) => !c.ids.includes(e.id));
      return before - s.entries.length;
    }
    case 'expire': {
      const e = db.get(c.key);
      if (!e) return 0;
      e.expiresAt = c.ttlMs === null ? null : Date.now() + c.ttlMs;
      return 1;
    }
    case 'rename': {
      const e = db.get(c.key);
      if (!e) throw ipcError('query', 'ERR no such key');
      db.delete(c.key);
      db.set(c.newKey, e);
      return 1;
    }
    case 'delete': {
      let n = 0;
      for (const k of c.keys) if (db.delete(k)) n++;
      return n;
    }
    case 'put':
      // Whole-key writes (import / restore) are only implemented by the real backend.
      throw ipcError('unsupported', 'key_put is not available in the browser mock', {
        capability: 'key_put',
      });
  }
}

/** Mock of `changes_preview` for `ChangeSet::Keys`. */
export function previewKeyChanges(set: KeyChangeSet): string[] {
  return set.changes.map(previewKeyChange);
}

/**
 * Mock of `changes_apply` for `ChangeSet::Keys`. Not atomic: a failing change leaves the
 * earlier ones applied and is reported as `changeFailed { index, applied }`. The `changes`
 * mock can delegate to this for the `keys` model.
 */
export function applyKeyChanges(set: KeyChangeSet): ApplyResult {
  const db = redisDb(redisDatabaseOf(set.database));
  const affected: number[] = [];
  for (const [index, change] of set.changes.entries()) {
    try {
      affected.push(applyOne(db, change));
    } catch (e) {
      const base = e as { message?: string };
      throw ipcError('changeFailed', base.message ?? 'change failed', {
        index,
        applied: affected.length,
      });
    }
  }
  return { applied: affected.length, affected, atomic: false };
}

// --- pattern delete job ------------------------------------------------------------------------

const CHUNK = 200;

function startDeleteJob(database: ObjectPath, pattern: string): JobId {
  const db = redisDb(redisDatabaseOf(database));
  const re = globToRegExp(pattern);
  const victims = [...db.keys()].filter((k) => re.test(k));
  const id = newId();
  const base: JobInfo = {
    id,
    kind: 'keyDelete',
    title: `DEL ${pattern}`,
    connectionId: null,
    status: 'running',
    progress: {
      phase: 'data',
      object: pattern,
      objectsDone: 0,
      objectsTotal: null,
      rowsDone: 0,
      rowsTotal: victims.length,
      bytesDone: 0,
      bytesTotal: null,
      message: null,
    },
    createdAt: new Date().toISOString(),
    startedAt: new Date().toISOString(),
    finishedAt: null,
    error: null,
    result: null,
    cancellable: true,
  };
  mockAddJob(base);
  let done = 0;
  const step = () => {
    void jobsMock.jobGet(id).then((job) => {
      if (job.status === 'cancelled') return; // `jobCancel` already announced the end
      for (const k of victims.slice(done, done + CHUNK)) db.delete(k);
      done = Math.min(victims.length, done + CHUNK);
      const progress = {
        ...base.progress,
        rowsDone: done,
        message: `${done}/${victims.length}`,
      };
      if (done < victims.length) {
        mockEmit('job://progress', { type: 'progress', jobId: id, progress });
        setTimeout(step, 5);
      } else {
        mockEmit('job://progress', {
          type: 'finished',
          job: {
            ...base,
            status: 'succeeded',
            progress,
            finishedAt: new Date().toISOString(),
            result: { deleted: done },
          },
        });
      }
    });
  };
  setTimeout(step, 5);
  return id;
}

export const keyvalueMock = {
  async objectChildrenPage(sessionId: Uuid, request: ChildrenPageRequest): Promise<ChildrenPage> {
    const kind = mockSessionKind(sessionId);
    const limit = clamp(request.limit, 1, 5000);
    const offset = Number(request.cursor ?? 0) || 0;
    const last = request.parent.segments[request.parent.segments.length - 1];
    if (kind === 'redis' && (last?.kind === 'database' || last?.kind === 'keyPrefix')) {
      // Fake SCAN: the cursor is an offset into the sorted matching keys; the pattern matches
      // the full key name. A prefix may come back in several batches.
      const keys = redisMatchingKeys(
        redisDatabaseOf(request.parent),
        redisPrefixOf(request.parent),
        request.pattern,
      );
      const batch = keys.slice(offset, offset + limit);
      const next = offset + batch.length;
      return {
        nodes: redisNodesForKeys(request.parent, batch),
        nextCursor: next < keys.length ? String(next) : null,
        approximateTotal: keys.length,
      };
    }
    const all =
      kind === 'redis' ? redisChildren(request.parent) : mockChildren(kind, request.parent);
    const re = request.pattern ? glob(request.pattern) : null;
    const filtered = re ? all.filter((n) => re.test(n.name)) : all;
    const nodes = filtered.slice(offset, offset + limit);
    const next = offset + nodes.length;
    return {
      nodes,
      nextCursor: next < filtered.length ? String(next) : null,
      approximateTotal: filtered.length,
    };
  },
  async keyRead(sessionId: Uuid, request: KeyReadRequest): Promise<KeyValuePage> {
    if (mockSessionKind(sessionId) !== 'redis') {
      throw ipcError('unsupported', 'key_value is not available for this connection', {
        capability: 'key_value',
      });
    }
    return readKeyPage(request);
  },
  async keysDeletePattern(
    sessionId: Uuid,
    database: ObjectPath,
    pattern: string,
    confirm: ConfirmationGrant | null,
  ): Promise<JobId> {
    if (mockSessionKind(sessionId) !== 'redis') {
      throw ipcError('unsupported', 'key_value is not available for this connection', {
        capability: 'key_value',
      });
    }
    if (requireConfirmation) {
      const confirmText = `DEL ${pattern}`;
      const expected = confirm ? pendingGrants.get(confirm.token) : undefined;
      if (!confirm || expected !== confirmText || confirm.typed !== confirmText) {
        const token = newId();
        pendingGrants.set(token, confirmText);
        throw ipcError('confirmationRequired', 'confirmation required', {
          confirmation: {
            token,
            confirmText,
            statements: [],
            expiresInMs: 60_000,
          },
        });
      }
      pendingGrants.delete(confirm.token);
    }
    return startDeleteJob(database, pattern);
  },
};
