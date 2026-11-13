import { QueryClient } from '@tanstack/react-query';
import { beforeEach, describe, expect, it } from 'vitest';
import { openRedisSession, resetRedisMocks } from '@/features/keyvalue/testUtils';
import { childPath, pathKey, ROOT_PATH } from '@/lib/objectPath';
import type { ObjectNode, ObjectPath } from '@/lib/types';
import { dedupeNodes, mergePage, TREE_PAGE_SIZE, type PagingState } from './paging';
import {
  clearBranch,
  getLevelPattern,
  loadChildren,
  loadMoreChildren,
  readChildrenState,
  readPaging,
  setLevelPattern,
} from './queries';
import { flattenTree, nodeActions, nodeLabel, type ChildrenState } from './treeModel';

const db0: ObjectPath = childPath(ROOT_PATH, 'database', '0');
const node = (parent: ObjectPath, kind: ObjectNode['kind'], name: string): ObjectNode => ({
  path: childPath(parent, kind, name),
  kind,
  name,
  hasChildren: kind !== 'key',
});

describe('mergePage', () => {
  const a = node(db0, 'keyPrefix', 'a:');
  const b = node(db0, 'keyPrefix', 'b:');
  const k = node(db0, 'key', 'k');

  it('appends new nodes and drops repeats by path', () => {
    expect(mergePage([a, k], [a, b]).map((n) => n.name)).toEqual(['a:', 'k', 'b:']);
  });
  it('returns the same array when nothing is new', () => {
    const existing = [a];
    expect(mergePage(existing, [a])).toBe(existing);
  });
  it('dedupes inside one page', () => {
    expect(dedupeNodes([a, a, b])).toHaveLength(2);
  });
});

describe('flattenTree with paging', () => {
  const a = node(db0, 'keyPrefix', 'a:');
  const getChildren = (p: ObjectPath): ChildrenState =>
    pathKey(p) === pathKey(ROOT_PATH)
      ? { status: 'loaded', nodes: [node(ROOT_PATH, 'database', '0')] }
      : pathKey(p) === pathKey(db0)
        ? { status: 'loaded', nodes: [a] }
        : { status: 'idle' };
  const paging = (patch: Partial<PagingState>): PagingState => ({
    nextCursor: '200',
    pattern: '',
    total: 500,
    loadingMore: false,
    ...patch,
  });
  const run = (state: PagingState | undefined) =>
    flattenTree({
      getChildren,
      getPaging: (p) => (pathKey(p) === pathKey(db0) ? state : undefined),
      expanded: new Set([pathKey(db0)]),
      label: (n) => n.name,
    }).map((r) => r.type);

  it('adds a "more" row while a cursor remains', () => {
    expect(run(paging({}))).toEqual(['node', 'node', 'more']);
    expect(run(paging({ nextCursor: null }))).toEqual(['node', 'node']);
  });
  it('shows the active pattern first', () => {
    expect(run(paging({ pattern: 'a*', nextCursor: null }))).toEqual(['node', 'pattern', 'node']);
  });
});

describe('node actions and labels', () => {
  it('offers key-level actions on database and prefix nodes of paged key-value drivers', async () => {
    const info = await openRedisSession();
    const caps = info.descriptor.capabilities;
    expect(nodeActions({ kind: 'keyPrefix', hasChildren: true }, caps)).toEqual([
      'copyName',
      'filterPattern',
      'deleteByPattern',
      'refresh',
    ]);
    expect(nodeActions({ kind: 'key', hasChildren: false }, caps)).toEqual(['openKey', 'copyName']);
    expect(
      nodeActions({ kind: 'database', hasChildren: true }, { ...caps, keyValueStore: false }),
    ).toEqual(['copyName', 'refresh']);
  });

  it('labels keys relative to their prefix folder and binary names compactly', () => {
    const prefix = node(db0, 'keyPrefix', 'user:');
    const t = ((k: string) => k) as never;
    expect(nodeLabel(t, node(prefix.path, 'keyPrefix', 'user:1:'))).toBe('1:');
    expect(nodeLabel(t, node(prefix.path, 'key', 'user:x'))).toBe('x');
    expect(nodeLabel(t, node(db0, 'key', '\u0000b64:3q2+7w=='))).toBe('<binary: 3q2+7w==>');
  });
});

describe('paged tree queries', () => {
  let qc: QueryClient;
  let sessionId: string;
  beforeEach(async () => {
    resetRedisMocks();
    qc = new QueryClient();
    sessionId = (await openRedisSession()).sessionId;
  });

  it('loads the first page, then appends the next ones without duplicates', async () => {
    const first = await loadChildren(qc, sessionId, db0, true);
    expect(first.length).toBeGreaterThan(0);
    expect(first.length).toBeLessThanOrEqual(TREE_PAGE_SIZE);
    const paging = readPaging(qc, sessionId, db0)!;
    expect(paging.nextCursor).toBe(String(TREE_PAGE_SIZE));
    expect(paging.total).toBeGreaterThan(2500);

    while (readPaging(qc, sessionId, db0)?.nextCursor) await loadMoreChildren(qc, sessionId, db0);
    const state = readChildrenState(qc, sessionId, db0);
    expect(state.status).toBe('loaded');
    const names = state.status === 'loaded' ? state.nodes.map((n) => n.name) : [];
    expect(new Set(names).size).toBe(names.length);
    expect(names).toEqual(expect.arrayContaining(['user:', 'session:', 'cache:', 'config']));
  });

  it('ignores loadMore when everything is loaded', async () => {
    await loadChildren(qc, sessionId, childPath(db0, 'keyPrefix', 'leaderboard:'), true);
    const path = childPath(db0, 'keyPrefix', 'leaderboard:');
    expect(readPaging(qc, sessionId, path)?.nextCursor).toBeNull();
    await loadMoreChildren(qc, sessionId, path);
    expect(readPaging(qc, sessionId, path)?.loadingMore).toBe(false);
  });

  it('changing the pattern resets the level and uses the new filter', async () => {
    await loadChildren(qc, sessionId, db0, true);
    expect(readPaging(qc, sessionId, db0)?.nextCursor).not.toBeNull();
    setLevelPattern(qc, sessionId, db0, 'leaderboard:*');
    expect(readChildrenState(qc, sessionId, db0).status).toBe('idle');
    expect(getLevelPattern(sessionId, db0)).toBe('leaderboard:*');
    const nodes = await loadChildren(qc, sessionId, db0, true);
    expect(nodes.map((n) => n.name)).toEqual(['leaderboard:']);
    expect(readPaging(qc, sessionId, db0)).toMatchObject({
      pattern: 'leaderboard:*',
      nextCursor: null,
    });
    // The filter survives a refresh of the branch and can be cleared.
    clearBranch(qc, sessionId, db0);
    expect(getLevelPattern(sessionId, db0)).toBe('leaderboard:*');
    setLevelPattern(qc, sessionId, db0, '');
    expect(getLevelPattern(sessionId, db0)).toBe('');
  });

  it('keeps load errors on the paging state', async () => {
    await loadChildren(qc, sessionId, db0, true);
    const { ipc } = await import('@/lib/ipc');
    const spy = (await import('vitest')).vi
      .spyOn(ipc, 'objectChildrenPage')
      .mockRejectedValueOnce({ kind: 'connection', message: 'boom' });
    await loadMoreChildren(qc, sessionId, db0);
    expect(readPaging(qc, sessionId, db0)).toMatchObject({
      loadingMore: false,
      moreError: { message: 'boom' },
    });
    spy.mockRestore();
    await loadMoreChildren(qc, sessionId, db0);
    expect(readPaging(qc, sessionId, db0)?.moreError).toBeUndefined();
  });
});
