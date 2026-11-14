// Per-tab state of the editors. The tab registry only renders the ACTIVE tab, so everything that
// must survive a tab switch (text, lease, results, transaction state) lives here, outside React.
// Entries are dropped, and their lease closed, when the tab leaves the tabs store.

import { create } from 'zustand';
import type { IpcError, LeaseState, StatementInfo, Uuid } from '@/lib/types';
import { useTabsStore } from '@/stores/tabs';
import type { ErrorMark } from './errorMarks';
import type { ParamDraft } from './params';
import type { StatementResult } from './results';

export type RunStatus = 'running' | 'ok' | 'error' | 'cancelled';

/** What the results panel shows: replaced (never mutated) whenever something changes. */
export interface RunSnapshot {
  id: number;
  status: RunStatus;
  queryId: Uuid;
  /** The text that was sent and where it starts in the document (scalar offset). */
  text: string;
  baseChar: number;
  results: StatementResult[];
  /** Messages outside any statement. */
  messages: string[];
  error: IpcError | null;
  errorMark: ErrorMark | null;
  startedAt: number;
  elapsedMs: number | null;
  /** The last result set can be continued with `lease_fetch_more`. */
  canFetchMore: boolean;
  fetchingMore: boolean;
  /** Whole document when the run started (error marks are valid only for it). */
  docAtStart: string;
}

export type LeaseStatus = 'none' | 'opening' | 'ready' | 'error';

export interface ConsoleEntry {
  id: number;
  command: string;
  run: RunSnapshot;
}

export interface EditorTabData {
  selectedResult?: { runId: number; key: string } | null;
  id: string;
  title: string;
  text: string;
  /** UTF-16 index, as persisted in `EditorTabState.cursor`. */
  cursor: number;
  selection: { from: number; to: number };
  filePath: string | null;
  /** Text as last saved to / read from `filePath`; `null` when there is no file. */
  savedText: string | null;
  /** Restored tab whose file had unsaved changes (the saved text is unknown). */
  restoredDirty: boolean;
  database: string | null;
  schema: string | null;
  /** Desired mode; applied to the lease when it opens, then mirrors `leaseState`. */
  autoCommit: boolean;
  leaseId: Uuid | null;
  leaseSessionId: Uuid | null;
  leaseStatus: LeaseStatus;
  leaseError: IpcError | null;
  leaseState: LeaseState | null;
  /** The connection was lost while a transaction was open: it is gone. */
  transactionLost: boolean;
  run: RunSnapshot | null;
  /** Last parameter values typed in the params dialog. */
  params: ParamDraft[];
  /** Lexer problems from `script_analyze` as editor marks. */
  problems: ErrorMark[];
  /** Redis / Mongo console scrollback. */
  console: ConsoleEntry[];
  consoleHistory: string[];
  scrollTop: number;
  scrollLeft: number;
  historyOpen: boolean;
  analyzedText: string | null;
  statements: StatementInfo[];
}

export function newTabData(id: string, init: Partial<EditorTabData> = {}): EditorTabData {
  return {
    id,
    title: '',
    text: '',
    cursor: 0,
    selection: { from: 0, to: 0 },
    filePath: null,
    savedText: null,
    restoredDirty: false,
    database: null,
    schema: null,
    autoCommit: true,
    leaseId: null,
    leaseSessionId: null,
    leaseStatus: 'none',
    leaseError: null,
    leaseState: null,
    transactionLost: false,
    run: null,
    params: [],
    problems: [],
    console: [],
    consoleHistory: [],
    scrollTop: 0,
    scrollLeft: 0,
    historyOpen: false,
    analyzedText: null,
    statements: [],
    ...init,
  };
}

interface EditorStore {
  tabs: Record<string, EditorTabData>;
  /** Create the entry when missing (keeps an existing one). */
  ensure: (id: string, init?: Partial<EditorTabData>) => EditorTabData;
  patch: (id: string, patch: Partial<EditorTabData>) => void;
  remove: (id: string) => void;
}

export const useEditorStore = create<EditorStore>((set, get) => ({
  tabs: {},
  ensure: (id, init) => {
    const existing = get().tabs[id];
    if (existing) return existing;
    const created = newTabData(id, init);
    set((s) => ({ tabs: { ...s.tabs, [id]: created } }));
    return created;
  },
  patch: (id, patch) =>
    set((s) => {
      const current = s.tabs[id];
      if (!current) return s;
      return { tabs: { ...s.tabs, [id]: { ...current, ...patch } } };
    }),
  remove: (id) =>
    set((s) => {
      if (!(id in s.tabs)) return s;
      const tabs = { ...s.tabs };
      delete tabs[id];
      return { tabs };
    }),
}));

export const getTabData = (id: string): EditorTabData | undefined =>
  useEditorStore.getState().tabs[id];

export const patchTabData = (id: string, patch: Partial<EditorTabData>): void =>
  useEditorStore.getState().patch(id, patch);

/** Hook: data of one editor, `undefined` before `ensure`. */
export const useTabData = (id: string): EditorTabData | undefined =>
  useEditorStore((s) => s.tabs[id]);

/** A transaction is (or may be) open: closing the tab must ask first (QUERY-11). */
export function hasOpenTransaction(data: EditorTabData | undefined): boolean {
  // A broken lease took its transaction with it (the server rolled it back).
  if (data?.leaseState?.broken) return false;
  const tx = data?.leaseState?.transaction;
  return tx === 'active' || tx === 'failed' || tx === 'unknown';
}

/** Whether the text differs from the file it came from. */
export const isDirty = (d: EditorTabData): boolean =>
  d.filePath !== null && (d.savedText !== null ? d.text !== d.savedText : d.restoredDirty);

// -- cleanup when a tab disappears ----------------------------------------------------------

type CleanupHook = (tabStateId: string) => void;
const cleanupHooks = new Set<CleanupHook>();

/** Called with the editor state id when its tab is gone (the runner closes leases here). */
export function onEditorTabRemoved(hook: CleanupHook): () => void {
  cleanupHooks.add(hook);
  return () => {
    cleanupHooks.delete(hook);
  };
}

const EDITOR_PREFIX = 'editor:';
const editorIds = (): Set<string> =>
  new Set(
    useTabsStore
      .getState()
      .tabs.filter((t) => t.kind === 'editor')
      .map((t) => t.id.slice(EDITOR_PREFIX.length)),
  );

let previousEditorIds = editorIds();
useTabsStore.subscribe(() => {
  const now = editorIds();
  for (const id of previousEditorIds) {
    if (!now.has(id)) {
      for (const hook of cleanupHooks) hook(id);
      useEditorStore.getState().remove(id);
    }
  }
  previousEditorIds = now;
});

/** Test helper: forget every editor. */
export function resetEditorStore(): void {
  useEditorStore.setState({ tabs: {} });
  previousEditorIds = editorIds();
}
