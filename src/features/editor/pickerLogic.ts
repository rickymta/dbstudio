// Pure logic of the editor's connection picker: which connections are offered, and the rebind of
// an editor tab onto another connection.

import type { KeyboardEvent } from 'react';
import { ipc } from '@/lib/ipc';
import type { ConnectionProfile, SessionInfo, Uuid } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { editorTabId, useTabsStore, type EditorMode } from '@/stores/tabs';
import { closeLease } from './editorRunner';
import { useDebugStore } from '@/features/debugger/debugStore';
import {
  getTabData,
  hasOpenTransaction,
  patchTabData,
  useTabData,
  type EditorTabData,
} from './editorStore';
import { modeOfKind } from './persist';

export interface PickerGroups {
  /** Connections that have a session. */
  open: ConnectionProfile[];
  /** The remaining saved connections. */
  other: ConnectionProfile[];
}

/**
 * Connections an editor of `mode` can switch to, split into those with a session and the rest,
 * each in the order of `profiles`; `filter` matches name or host, case-insensitively.
 */
export function groupConnections(
  profiles: readonly ConnectionProfile[],
  sessions: Record<Uuid, SessionInfo>,
  mode: EditorMode,
  filter = '',
): PickerGroups {
  const needle = filter.trim().toLowerCase();
  const groups: PickerGroups = { open: [], other: [] };
  for (const p of profiles) {
    if (modeOfKind(p.kind) !== mode) continue;
    if (needle && !`${p.name} ${p.host}`.toLowerCase().includes(needle)) continue;
    (sessions[p.id] ? groups.open : groups.other).push(p);
  }
  return groups;
}

export type SwitchBlock = 'busy' | 'transaction' | 'debugging';

function blockOf(data: EditorTabData | undefined, debugging: boolean): SwitchBlock | null {
  if (debugging) return 'debugging';
  if (data?.run?.status === 'running' || data?.run?.fetchingMore) return 'busy';
  if (hasOpenTransaction(data)) return 'transaction';
  return null;
}

const isDebugging = (tabStateId: string): boolean => {
  const debug = useDebugStore.getState();
  return !!debug.sessions[tabStateId] || debug.starting[tabStateId] === true;
};

/** Why a tab cannot switch connection or database right now, or `null`. */
export function switchBlock(tabStateId: string): SwitchBlock | null {
  return blockOf(getTabData(tabStateId), isDebugging(tabStateId));
}

/** Reactive [`switchBlock`] for the pickers. */
export function useSwitchBlock(tabStateId: string): SwitchBlock | null {
  const data = useTabData(tabStateId);
  const debugging = useDebugStore(
    (s) => !!s.sessions[tabStateId] || s.starting[tabStateId] === true,
  );
  return blockOf(data, debugging);
}

/**
 * Point an editor at `connectionId`: its lease (on the old session) is closed, the context and
 * everything derived from the old connection is reset; text, title, file and results stay.
 * Returns the session of the new connection when it is already open. `false` when blocked.
 */
export async function rebindEditor(
  tabStateId: string,
  connectionId: Uuid,
): Promise<{ session: SessionInfo | undefined } | false> {
  if (switchBlock(tabStateId)) return false;
  const tabId = editorTabId(tabStateId);
  const tab = useTabsStore.getState().tabs.find((t) => t.id === tabId);
  if (tab?.kind !== 'editor' || tab.payload.connectionId === connectionId) return false;
  const session = useSessionsStore.getState().sessionsByConnection[connectionId];
  // Rebind first: the lease is cleared synchronously by `closeLease`, only the server call waits.
  const closing = closeLease(tabStateId);
  useTabsStore
    .getState()
    .rebindEditor(tabId, { connectionId, sessionId: session?.sessionId ?? null });
  patchTabData(tabStateId, {
    database: null,
    schema: null,
    problems: [],
    statements: [],
    analyzedText: null,
    leaseError: null,
    transactionLost: false,
  });
  await closing;
  return { session };
}

/** Databases directly under the root of a session (empty when it has no database level). */
export async function listDatabases(sessionId: Uuid): Promise<string[]> {
  const nodes = await ipc.objectChildren(sessionId, { segments: [] });
  return nodes.filter((n) => n.kind === 'database').map((n) => n.name);
}

/** Keep typing in the filter box from reaching the menu (type-ahead), except keys that navigate. */
export function filterKeyDown(e: KeyboardEvent<HTMLInputElement>): void {
  if (!['ArrowDown', 'ArrowUp', 'Escape', 'Tab'].includes(e.key)) e.stopPropagation();
}
