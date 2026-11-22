// What the Launchpad lists, gathered from the stores (pure: inputs in, items out, so it is tested
// without rendering). Unfinished work comes from the tabs of every workspace, not only the one on
// screen: an open transaction in a hidden workspace still holds locks.

import { countChanges, type PendingTab } from '@/features/data/edit/pendingChanges';
import { hasOpenTransaction, isDirty, type EditorTabData } from '@/features/editor/editorStore';
import type { Uuid } from '@/lib/types';
import { NO_SCOPE, type TabItem, type TabSet } from '@/stores/tabs';

export type WorkKind = 'transaction' | 'running' | 'unsavedFile' | 'pendingEdits';

/** Order of the kinds in the list: what holds server resources first. */
export const WORK_ORDER: WorkKind[] = ['transaction', 'running', 'pendingEdits', 'unsavedFile'];

export interface WorkItem {
  key: string;
  kind: WorkKind;
  /** Tab to show when the item is opened. */
  tabId: string;
  /** Workspace whose tab set holds the tab ([`NO_SCOPE`] for none). */
  scope: string;
  title: TabItem['title'];
  connectionId: Uuid | null;
  /** Editor state id (transactions, running statements, files). */
  tabStateId?: string;
  sessionId?: Uuid | null;
  /** Pending changes (pendingEdits) or the transaction state (transaction). */
  count?: number;
  state?: string;
  /** Since when the statement runs (ms since epoch), for `running`. */
  since?: number;
}

export interface WorkInputs {
  tabs: TabItem[];
  scope: string;
  stashed: Record<string, TabSet>;
  editors: Record<string, EditorTabData>;
  pending: Record<string, PendingTab>;
}

/** Every tab with the scope (workspace) holding it. */
function tabsWithScope(input: Pick<WorkInputs, 'tabs' | 'scope' | 'stashed'>) {
  const out: { tab: TabItem; scope: string }[] = input.tabs.map((tab) => ({
    tab,
    scope: input.scope,
  }));
  for (const [scope, set] of Object.entries(input.stashed))
    for (const tab of set.tabs) out.push({ tab, scope });
  return out;
}

/** Editor tab owning a result grid id (`<tab state id>:<run id>:r<n>`), if it is one. */
const editorOfGrid = (gridId: string) => /^(.+):\d+:r\d+$/.exec(gridId)?.[1];

export function collectWork(input: WorkInputs): WorkItem[] {
  const all = tabsWithScope(input);
  const byId = new Map(all.map((x) => [x.tab.id, x]));
  const items: WorkItem[] = [];
  for (const { tab, scope } of all) {
    if (tab.kind !== 'editor') continue;
    const data = input.editors[tab.payload.tabStateId];
    if (!data) continue;
    const base = {
      tabId: tab.id,
      scope,
      title: data.title || tab.title,
      connectionId: tab.payload.connectionId,
      tabStateId: tab.payload.tabStateId,
      sessionId: tab.payload.sessionId,
    };
    if (hasOpenTransaction(data)) {
      items.push({
        ...base,
        key: `tx:${tab.id}`,
        kind: 'transaction',
        state: data.leaseState?.transaction ?? undefined,
      });
    }
    if (data.run?.status === 'running') {
      items.push({ ...base, key: `run:${tab.id}`, kind: 'running', since: data.run.startedAt });
    }
    if (isDirty(data) && data.filePath) {
      items.push({ ...base, key: `file:${tab.id}`, kind: 'unsavedFile', state: data.filePath });
    }
  }
  // Pending edits: of a data tab, or of a result grid inside an editor tab (one item per tab).
  const pendingByTab = new Map<string, number>();
  for (const [gridId, p] of Object.entries(input.pending)) {
    const n = countChanges(p);
    if (n === 0) continue;
    const owner = byId.has(gridId) ? gridId : `editor:${editorOfGrid(gridId) ?? ''}`;
    if (!byId.has(owner)) continue;
    pendingByTab.set(owner, (pendingByTab.get(owner) ?? 0) + n);
  }
  for (const [tabId, count] of pendingByTab) {
    const { tab, scope } = byId.get(tabId)!;
    items.push({
      key: `edits:${tabId}`,
      kind: 'pendingEdits',
      tabId,
      scope,
      title: tab.title,
      connectionId: tab.payload?.connectionId ?? null,
      count,
    });
  }
  return items.sort((a, b) => WORK_ORDER.indexOf(a.kind) - WORK_ORDER.indexOf(b.kind));
}

/** Items of the Launchpad scope: the active workspace's tabs, or everything. */
export function inScope<T extends { scope?: string; connectionId?: Uuid | null }>(
  items: T[],
  opts: { all: boolean; workspace: string; members: ReadonlySet<Uuid> | null },
  by: 'tab' | 'connection',
): T[] {
  if (opts.all || !opts.workspace) return items;
  return by === 'tab'
    ? items.filter((i) => (i.scope ?? NO_SCOPE) === opts.workspace)
    : items.filter((i) => !i.connectionId || !!opts.members?.has(i.connectionId));
}

/** `5s`, `3m`, `2h`, `4d` since `ms`. */
export function ago(ms: number, now = Date.now()): string {
  const s = Math.max(0, Math.round((now - ms) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}
