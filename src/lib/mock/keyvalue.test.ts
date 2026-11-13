import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ipc } from '@/lib/ipc';
import { childPath, ROOT_PATH } from '@/lib/objectPath';
import { keyPath, openRedisSession, resetRedisMocks } from '@/features/keyvalue/testUtils';
import type { JobEvent, KeyChange, ObjectNode, ObjectPath, Value } from '@/lib/types';
import { mockListen, resetMockEvents } from './events';
import { redisChildren, redisDb } from './catalogRedis';
import { applyKeyChanges, previewKeyChanges, setMockKeyDeleteConfirmation } from './keyvalue';

const db0: ObjectPath = { segments: [{ kind: 'database', name: '0' }] };
const prefix = (name: string): ObjectPath => childPath(db0, 'keyPrefix', name);
const text = (v: string): Value => ({ t: 'text', v });

let sessionId: string;
beforeEach(async () => {
  resetRedisMocks();
  sessionId = (await openRedisSession()).sessionId;
});
afterEach(() => {
  setMockKeyDeleteConfirmation(false);
  resetMockEvents();
});

describe('mock Redis catalog', () => {
  it('holds thousands of keys of all six types', () => {
    const keys = [...redisDb('0').values()];
    expect(keys.length).toBeGreaterThan(2500);
    const types = new Set(keys.map((k) => k.type));
    for (const t of ['string', 'hash', 'list', 'set', 'zset', 'stream']) expect(types).toContain(t);
    expect(redisDb('0').has('user:1:profile')).toBe(true);
    expect([...redisDb('0').keys()].some((k) => k.startsWith('session:'))).toBe(true);
  });

  it('lists databases and one level of the key tree', () => {
    expect(redisChildren(ROOT_PATH).map((n) => n.name)).toEqual(['0', '1']);
    const level = redisChildren(db0);
    expect(level.filter((n) => n.kind === 'keyPrefix').map((n) => n.name)).toEqual(
      expect.arrayContaining(['user:', 'session:', 'leaderboard:']),
    );
    expect(level.find((n) => n.name === 'config')).toMatchObject({ kind: 'key', detail: 'string' });
  });
});

describe('objectChildrenPage (fake SCAN)', () => {
  async function drain(parent: ObjectPath, limit: number, pattern?: string) {
    const nodes: ObjectNode[] = [];
    let cursor: string | null = null;
    let pages = 0;
    do {
      const page = await ipc.objectChildrenPage(sessionId, { parent, cursor, limit, pattern });
      nodes.push(...page.nodes);
      cursor = page.nextCursor;
      pages++;
    } while (cursor);
    return { nodes, pages };
  }

  it('pages with a cursor and ends with a null cursor', async () => {
    const first = await ipc.objectChildrenPage(sessionId, { parent: db0, limit: 100 });
    expect(first.nextCursor).not.toBeNull();
    expect(first.approximateTotal).toBeGreaterThan(2500);
    const { pages } = await drain(db0, 1000);
    expect(pages).toBeGreaterThan(2);
  });

  it('can return the same key prefix on several pages', async () => {
    const { nodes } = await drain(db0, 100);
    const names = nodes.filter((n) => n.kind === 'keyPrefix').map((n) => n.name);
    expect(names.length).toBeGreaterThan(new Set(names).size);
  });

  it('lists keys below a prefix with their type and ttl', async () => {
    const page = await ipc.objectChildrenPage(sessionId, { parent: prefix('user:1:'), limit: 50 });
    expect(page.nodes.map((n) => n.name).sort()).toEqual(['user:1:profile', 'user:1:recent']);
    expect(page.nodes.every((n) => n.kind === 'key')).toBe(true);
    const sessions = await ipc.objectChildrenPage(sessionId, {
      parent: prefix('session:'),
      limit: 3,
    });
    expect(sessions.nodes[0]!.detail).toMatch(/^string ttl:\d+$/);
  });

  it('filters by glob pattern on the full key name', async () => {
    const { nodes } = await drain(db0, 500, 'leaderboard:*');
    expect(nodes.map((n) => n.name)).toEqual(['leaderboard:']);
    const keys = await drain(prefix('leaderboard:'), 500, '*daily');
    expect(keys.nodes.map((n) => n.name)).toEqual(['leaderboard:daily']);
  });

  it('still pages the plain tree of other drivers', async () => {
    const pg = (await ipc.sessionOpen('00000000-0000-4000-8000-0000000000b1')).sessionId;
    const page = await ipc.objectChildrenPage(pg, { parent: ROOT_PATH, limit: 1 });
    expect(page.nodes).toHaveLength(1);
  });
});

describe('keyRead', () => {
  const read = (name: string) => ipc.keyRead(sessionId, { key: keyPath(name), count: 5 });

  it('reads every type', async () => {
    expect((await read('config')).entries).toMatchObject({ type: 'string', truncated: false });
    expect((await read('user:1:profile')).entries).toMatchObject({ type: 'hash' });
    expect((await read('user:1:recent')).entries).toMatchObject({ type: 'list', start: 0 });
    expect((await read('user:3:tags')).entries).toMatchObject({ type: 'set' });
    expect((await read('leaderboard:daily')).entries).toMatchObject({ type: 'zSet' });
    expect((await read('stream:orders')).entries).toMatchObject({ type: 'stream', groups: 2 });
    const json = await read('doc:settings');
    expect(json.keyType).toBe('ReJSON-RL');
    expect(json.entries.type).toBe('json');
  });

  it('pages by cursor and honours the pattern', async () => {
    const key = keyPath('leaderboard:daily');
    const first = await ipc.keyRead(sessionId, { key, count: 100 });
    expect(first.length).toBe(250);
    expect(first.nextCursor).toBe('100');
    const second = await ipc.keyRead(sessionId, { key, count: 100, cursor: first.nextCursor });
    expect(second.nextCursor).toBe('200');
    const filtered = await ipc.keyRead(sessionId, { key, count: 100, pattern: 'player-1?' });
    expect(filtered.entries.type === 'zSet' && filtered.entries.members).toHaveLength(10);
    expect(filtered.nextCursor).toBeNull();
  });

  it('reports ttl, truncation and missing keys', async () => {
    const ttl = await read('cache:product:1');
    expect(ttl.ttlMs).toBeGreaterThan(0);
    const cut = await ipc.keyRead(sessionId, {
      key: keyPath('cache:product:1'),
      count: 1,
      maxStringBytes: 4,
    });
    expect(cut.entries).toMatchObject({ type: 'string', truncated: true });
    await expect(read('nope')).rejects.toMatchObject({ kind: 'notFound' });
  });
});

describe('ChangeSet::Keys in the mock', () => {
  const set = (changes: KeyChange[]) => ({ database: db0, changes });

  it('applies changes and previews commands', () => {
    const changes: KeyChange[] = [
      { op: 'hashSet', key: 'user:1:profile', entries: [[text('name'), text('Zed')]] },
      { op: 'expire', key: 'user:1:profile', ttlMs: 10_000 },
    ];
    expect(previewKeyChanges(set(changes))).toEqual([
      'HSET user:1:profile name Zed',
      'PEXPIRE user:1:profile 10000',
    ]);
    expect(applyKeyChanges(set(changes))).toMatchObject({ applied: 2, atomic: false });
    const entry = redisDb('0').get('user:1:profile')!;
    expect(entry.expiresAt).not.toBeNull();
    expect(entry.type === 'hash' && entry.fields[0]![1]).toEqual(text('Zed'));
  });

  it('keeps the ttl on setString without ttlMs', () => {
    const before = redisDb('0').get('cache:product:2')!.expiresAt;
    applyKeyChanges(set([{ op: 'setString', key: 'cache:product:2', value: text('x') }]));
    expect(redisDb('0').get('cache:product:2')!.expiresAt).toBe(before);
  });

  it('is not atomic: a failure reports index and applied count', () => {
    let error: unknown;
    try {
      applyKeyChanges(
        set([
          { op: 'setAdd', key: 'user:3:tags', members: [text('new')] },
          { op: 'listSet', key: 'user:1:recent', index: 999, value: text('x') },
          { op: 'delete', keys: ['config'] },
        ]),
      );
    } catch (e) {
      error = e;
    }
    expect(error).toMatchObject({ kind: 'changeFailed', index: 1, applied: 1 });
    expect(redisDb('0').has('config')).toBe(true);
  });

  it('renames and deletes', () => {
    applyKeyChanges(set([{ op: 'rename', key: 'config', newKey: 'config2' }]));
    expect(redisDb('0').has('config2')).toBe(true);
    applyKeyChanges(set([{ op: 'delete', keys: ['config2'] }]));
    expect(redisDb('0').has('config2')).toBe(false);
  });
});

describe('keysDeletePattern', () => {
  async function runJob(pattern: string): Promise<JobEvent[]> {
    const events: JobEvent[] = [];
    const off = mockListen('job://progress', (e) => events.push(e as JobEvent));
    await ipc.keysDeletePattern(sessionId, db0, pattern);
    await vi.waitFor(() => expect(events.some((e) => e.type === 'finished')).toBe(true), {
      timeout: 3000,
    });
    off();
    return events;
  }

  it('deletes matching keys through a keyDelete job with progress', async () => {
    const before = redisDb('0').size;
    const events = await runJob('session:*');
    expect(events[0]).toMatchObject({ type: 'started', job: { kind: 'keyDelete' } });
    expect(events.some((e) => e.type === 'progress')).toBe(true);
    const last = events[events.length - 1]!;
    expect(last).toMatchObject({ type: 'finished', job: { status: 'succeeded' } });
    expect(redisDb('0').size).toBe(before - 900);
    expect(redisDb('0').has('config')).toBe(true);
  });

  it('asks for confirmation on protected connections and accepts the grant', async () => {
    setMockKeyDeleteConfirmation(true);
    const err = await ipc.keysDeletePattern(sessionId, db0, 'jobs:*').catch((e: unknown) => e);
    expect(err).toMatchObject({
      kind: 'confirmationRequired',
      confirmation: { confirmText: 'DEL jobs:*' },
    });
    const { token, confirmText } = (err as { confirmation: { token: string; confirmText: string } })
      .confirmation;
    await expect(
      ipc.keysDeletePattern(sessionId, db0, 'jobs:*', { token, typed: 'wrong' }),
    ).rejects.toMatchObject({ kind: 'confirmationRequired' });
    const id = await ipc.keysDeletePattern(sessionId, db0, 'jobs:*', { token, typed: confirmText });
    expect(typeof id).toBe('string');
  });
});
