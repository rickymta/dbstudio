import { describe, expect, it } from 'vitest';
import type { EditorTabData } from '@/features/editor/editorStore';
import type { PendingTab } from '@/features/data/edit/pendingChanges';
import { NO_SCOPE, WELCOME_TAB, type TabItem } from '@/stores/tabs';
import { ago, collectWork, inScope } from './model';

const editor = (id: string): TabItem => ({
  id: `editor:${id}`,
  kind: 'editor',
  title: id,
  closable: true,
  payload: { tabStateId: id, connectionId: 'c1', sessionId: 's1', mode: 'sql' },
});
const table: TabItem = {
  id: 'table:t',
  kind: 'table',
  title: 'orders',
  closable: true,
  payload: { sessionId: 's1', connectionId: 'c2', path: { segments: [] } },
};
const data = (over: Partial<EditorTabData>): EditorTabData =>
  ({ title: '', text: '', savedText: null, filePath: null, ...over }) as EditorTabData;
const pending = (n: number): PendingTab =>
  ({
    updates: Object.fromEntries(Array.from({ length: n }, (_, i) => [String(i), {}])),
    deletes: {},
    inserts: [],
  }) as unknown as PendingTab;

describe('Launchpad unfinished work', () => {
  it('lists transactions, running statements, unsaved files and edits from every workspace', () => {
    const items = collectWork({
      tabs: [WELCOME_TAB, editor('a'), table],
      scope: NO_SCOPE,
      stashed: { w1: { tabs: [WELCOME_TAB, editor('b')], activeId: WELCOME_TAB.id } },
      editors: {
        a: data({ leaseState: { transaction: 'active' } as EditorTabData['leaseState'] }),
        b: data({
          run: { status: 'running', startedAt: 1000 } as EditorTabData['run'],
          filePath: '/x.sql',
          text: 'changed',
          savedText: 'old',
        }),
      },
      pending: { 'table:t': pending(2), 'a:3:r0': pending(1), 'gone:1:r0': pending(5) },
    });
    expect(items.map((i) => `${i.kind}:${i.tabId}:${i.scope}`)).toEqual([
      'transaction:editor:a:',
      'running:editor:b:w1',
      'pendingEdits:table:t:',
      'pendingEdits:editor:a:',
      'unsavedFile:editor:b:w1',
    ]);
    expect(items.find((i) => i.tabId === 'table:t')!.count).toBe(2);
  });

  it('keeps the active workspace or everything', () => {
    const items = [
      { scope: 'w1', connectionId: 'a' },
      { scope: NO_SCOPE, connectionId: 'b' },
    ];
    const members = new Set(['a']);
    expect(inScope(items, { all: false, workspace: 'w1', members }, 'tab')).toHaveLength(1);
    expect(inScope(items, { all: true, workspace: 'w1', members }, 'tab')).toHaveLength(2);
    expect(inScope(items, { all: false, workspace: 'w1', members }, 'connection')).toEqual([
      items[0],
    ]);
    expect(inScope(items, { all: false, workspace: '', members: null }, 'connection')).toHaveLength(
      2,
    );
  });

  it('formats ages', () => {
    expect(ago(0, 30_000)).toBe('30s');
    expect(ago(0, 5 * 60_000)).toBe('5m');
    expect(ago(0, 3 * 3600_000)).toBe('3h');
    expect(ago(0, 2 * 86400_000)).toBe('2d');
  });
});
