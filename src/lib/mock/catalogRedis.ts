// In-memory fake Redis for the browser mock: a few thousand keys of every type in two
// databases. `keyvalue.ts` serves paged tree and key reads from it; `redisChildren` backs the
// plain (unpaged) children list.

import { childPath, lastSegment, ROOT_PATH } from '../objectPath';
import type {
  ObjectKind,
  ObjectNode,
  ObjectPath,
  StreamEntry,
  TableDetails,
  Value,
} from '../types';
import { ipcError } from './util';

export const REDIS_DELIMITER = ':';
/** Same prefix as `encode_key_name` in the core for binary key names. */
const BINARY_PREFIX = '\u0000b64:';

export type MockRedisValue =
  | { type: 'string'; value: Value }
  | { type: 'hash'; fields: [Value, Value][] }
  | { type: 'list'; items: Value[] }
  | { type: 'set'; members: Value[] }
  | { type: 'zset'; members: [Value, string][] }
  | { type: 'stream'; entries: StreamEntry[]; groups: number }
  | { type: 'json'; text: string };

export type MockRedisKey = MockRedisValue & {
  /** Absolute expiry in epoch ms; `null` = persistent. */
  expiresAt: number | null;
};

const text = (v: string): Value => ({ t: 'text', v });

function seedDatabase0(): Map<string, MockRedisKey> {
  const keys = new Map<string, MockRedisKey>();
  const put = (name: string, value: MockRedisValue, ttlSeconds: number | null = null) =>
    keys.set(name, {
      ...value,
      expiresAt: ttlSeconds === null ? null : Date.now() + ttlSeconds * 1000,
    });

  for (let i = 1; i <= 600; i++) {
    put(`user:${i}:profile`, {
      type: 'hash',
      fields: [
        [text('name'), text(`User ${i}`)],
        [text('email'), text(`user${i}@example.com`)],
        [text('plan'), text(i % 5 === 0 ? 'pro' : 'free')],
        [text('created'), text(`2026-01-${String((i % 28) + 1).padStart(2, '0')}`)],
      ],
    });
    put(`user:${i}:recent`, {
      type: 'list',
      items: Array.from({ length: (i % 7) + 2 }, (_, n) => text(`/page/${i}/${n}`)),
    });
    if (i % 3 === 0) {
      put(`user:${i}:tags`, {
        type: 'set',
        members: ['alpha', 'beta', 'gamma', `team-${i % 9}`].map(text),
      });
    }
  }
  for (let i = 1; i <= 900; i++) {
    put(
      `session:${(100000 + i * 7919).toString(16)}`,
      {
        type: 'string',
        value: text(JSON.stringify({ user: (i % 600) + 1, ip: `10.0.${i % 255}.${i % 200}` })),
      },
      60 + i * 13,
    );
  }
  for (let i = 1; i <= 400; i++) {
    put(
      `cache:product:${i}`,
      {
        type: 'string',
        value: text(JSON.stringify({ id: i, name: `Product ${i}`, price: i * 3.5 })),
      },
      300 + i,
    );
  }
  for (const board of ['daily', 'weekly', 'alltime']) {
    put(`leaderboard:${board}`, {
      type: 'zset',
      members: Array.from({ length: 250 }, (_, n): [Value, string] => [
        text(`player-${n}`),
        String(1000 - n * 3 + (n % 4)),
      ]),
    });
  }
  for (const name of ['orders', 'audit']) {
    put(`stream:${name}`, {
      type: 'stream',
      groups: name === 'orders' ? 2 : 0,
      entries: Array.from({ length: 120 }, (_, n) => ({
        id: `${1700000000000 + n * 1000}-0`,
        fields: [
          [text('type'), text(n % 2 ? 'created' : 'paid')],
          [text('order'), text(String(5000 + n))],
        ] as [Value, Value][],
      })),
    });
  }
  put('config', { type: 'string', value: text('{"featureX":true,"limit":50}') });
  put('config:motd', { type: 'string', value: text('Welcome to DBStudio') });
  put('counter:visits', { type: 'string', value: { t: 'text', v: '123456' } });
  put('doc:settings', { type: 'json', text: '{"theme":"dark","beta":[1,2,3]}' });
  put(`${BINARY_PREFIX}3q2+7w==`, {
    type: 'string',
    value: { t: 'bytes', v: '3q2+78vv/g==' },
  });
  return keys;
}

function seedDatabase1(): Map<string, MockRedisKey> {
  const keys = new Map<string, MockRedisKey>();
  for (let i = 1; i <= 40; i++) {
    keys.set(`jobs:queue:${i}`, {
      type: 'list',
      items: [text(`job-${i}-a`), text(`job-${i}-b`)],
      expiresAt: null,
    });
  }
  keys.set('jobs:lock', {
    type: 'string',
    value: text('worker-1'),
    expiresAt: Date.now() + 30_000,
  });
  return keys;
}

let databases = new Map<string, Map<string, MockRedisKey>>([
  ['0', seedDatabase0()],
  ['1', seedDatabase1()],
]);

export function resetMockRedis(): void {
  databases = new Map([
    ['0', seedDatabase0()],
    ['1', seedDatabase1()],
  ]);
}

/** Keys of database `name`; expired keys are dropped lazily like Redis does. */
export function redisDb(name: string): Map<string, MockRedisKey> {
  const db = databases.get(name);
  if (!db) throw ipcError('notFound', `database ${name}`);
  const now = Date.now();
  for (const [k, v] of db) if (v.expiresAt !== null && v.expiresAt <= now) db.delete(k);
  return db;
}

/** Redis-style glob (`*`, `?`, `[...]`, `\` escape); case-sensitive like `SCAN MATCH`. */
export function globToRegExp(pattern: string): RegExp {
  let out = '^';
  for (let i = 0; i < pattern.length; i++) {
    const c = pattern[i]!;
    if (c === '*') out += '[\\s\\S]*';
    else if (c === '?') out += '[\\s\\S]';
    else if (c === '[') {
      const end = pattern.indexOf(']', i + 1);
      if (end < 0) out += '\\[';
      else {
        const body = pattern.slice(i + 1, end).replace(/\\/g, '\\\\');
        out += body.startsWith('^') ? `[^${body.slice(1)}]` : `[${body}]`;
        i = end;
      }
    } else if (c === '\\' && i + 1 < pattern.length) out += `\\${pattern[++i]}`;
    else out += c.replace(/[.+^${}()|\\]/g, '\\$&');
  }
  return new RegExp(`${out}$`);
}

export function redisDatabaseOf(path: ObjectPath): string {
  const db = path.segments.find((s) => s.kind === 'database');
  if (!db) throw ipcError('notFound', 'database segment missing');
  return db.name;
}

/** Node of a key (`key` kind) or virtual folder (`keyPrefix` kind) below `parent`. */
function levelNode(parent: ObjectPath, db: string, prefix: string, name: string): ObjectNode {
  const rest = name.slice(prefix.length);
  const cut = rest.indexOf(REDIS_DELIMITER);
  if (cut >= 0) {
    return node(parent, 'keyPrefix', prefix + rest.slice(0, cut + 1), true);
  }
  const entry = redisDb(db).get(name)!;
  const ttl =
    entry.expiresAt === null
      ? ''
      : ` ttl:${Math.max(0, Math.round((entry.expiresAt - Date.now()) / 1000))}`;
  return node(parent, 'key', name, false, `${entry.type}${ttl}`);
}

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

/** Sorted keys of `db` that start with `prefix` and match `pattern` (full key name). */
export function redisMatchingKeys(db: string, prefix: string, pattern?: string | null): string[] {
  const re = pattern ? globToRegExp(pattern) : null;
  const out: string[] = [];
  for (const k of redisDb(db).keys()) {
    if (k.startsWith(prefix) && (!re || re.test(k))) out.push(k);
  }
  return out.sort();
}

/** The prefix whose children `path` lists: `""` for a database, the folder name for a prefix. */
export function redisPrefixOf(path: ObjectPath): string {
  const last = lastSegment(path);
  return last?.kind === 'keyPrefix' ? last.name : '';
}

/**
 * Nodes for a slice of matching keys, like one SCAN batch: a prefix shows up once per batch,
 * and again in later batches when its keys span several of them.
 */
export function redisNodesForKeys(path: ObjectPath, keys: string[]): ObjectNode[] {
  const db = redisDatabaseOf(path);
  const prefix = redisPrefixOf(path);
  const seen = new Set<string>();
  const out: ObjectNode[] = [];
  for (const k of keys) {
    const n = levelNode(path, db, prefix, k);
    if (seen.has(n.name)) continue;
    seen.add(n.name);
    out.push(n);
  }
  return out;
}

/** Children of `path`: databases -> virtual key-prefix folders and keys. */
export function redisChildren(path: ObjectPath): ObjectNode[] {
  const last = lastSegment(path);
  if (!last) return ['0', '1'].map((d) => node(ROOT_PATH, 'database', d, true));
  if (last.kind !== 'database' && last.kind !== 'keyPrefix') return [];
  const keys = redisMatchingKeys(redisDatabaseOf(path), redisPrefixOf(path));
  const nodes = redisNodesForKeys(path, keys);
  // Folders first, like the real explorer.
  return [...nodes.filter((n) => n.kind === 'keyPrefix'), ...nodes.filter((n) => n.kind === 'key')];
}

export function redisDetails(path: ObjectPath): TableDetails {
  void path;
  throw ipcError('unsupported', 'table details are not available in the browser mock');
}

export function redisDdl(path: ObjectPath): string {
  return `-- DDL is not available for ${lastSegment(path)?.name ?? ''}`;
}
