// Frontend state of the SQL debugger (design 5.7): live sessions per editor tab, breakpoints and
// watches that survive sessions, and the few global preferences. Pure state; side effects
// (IPC, persistence) live in `debugActions.ts`.

import { create } from 'zustand';
import {
  DEFAULT_DEBUG_OPTIONS,
  type Breakpoint,
  type DebugAction,
  type DebugKeymap,
  type DebugOptions,
  type DebugState,
  type DebugVariable,
  type RoutineRef,
  type SourceRef,
  type Uuid,
  type WatchSpec,
} from '@/lib/types';
import type { OutputItem } from './consoleModel';

export const OUTPUT_LIMIT = 2000;

export const scriptScope = (tabId: string): string => `script:${tabId}`;
export const routineScope = (connectionId: Uuid, signature: string): string =>
  `routine:${connectionId}:${signature}`;

/** A place breakpoints live in: the script of an editor tab or the source of a routine. */
export interface ScopeInfo {
  source: SourceRef;
  /** Editor tab (script scopes). */
  tabId?: string;
  /** Connection (routine scopes: breakpoints are saved per connection). */
  connectionId?: Uuid;
  routine?: RoutineRef;
  sourceHash?: string | null;
  breakpoints: Breakpoint[];
}

export interface DebugSession {
  debugId: Uuid;
  state: DebugState;
  outputs: OutputItem[];
  /** Last `seq` seen on events (gap detection). */
  lastSeq: number;
  lastOutputSeq: number;
  /** A command is in flight: the step buttons are disabled. */
  busy: boolean;
  options: DebugOptions;
  /** Action that led to the current pause (a confirmation retries it). */
  lastAction: DebugAction | null;
  lastStepMs: number | null;
}

export type ToolPanel = 'debugger' | 'console';
export type BottomView = 'results' | 'debug';

export interface TabDebugUi {
  panel: ToolPanel;
  bottom: BottomView;
}

const DEFAULT_UI: TabDebugUi = { panel: 'debugger', bottom: 'results' };

export interface DebugStoreState {
  sessions: Record<string, DebugSession>;
  starting: Record<string, boolean>;
  scopes: Record<string, ScopeInfo>;
  watches: Record<string, WatchSpec[]>;
  muted: Record<string, boolean>;
  ui: Record<string, TabDebugUi>;
  /** Children loaded by the Variables tree, by tab then reference (hover looks here). */
  children: Record<string, Record<string, DebugVariable[]>>;
  keymap: DebugKeymap;
  inlineValues: boolean;
  startOptions: DebugOptions;

  ensureScope: (key: string, info: Omit<ScopeInfo, 'breakpoints'>) => void;
  setBreakpoints: (key: string, breakpoints: Breakpoint[]) => void;
  setWatches: (tabId: string, watches: WatchSpec[]) => void;
  setMuted: (tabId: string, muted: boolean) => void;
  setSession: (tabId: string, session: DebugSession | null) => void;
  patchSession: (tabId: string, patch: Partial<DebugSession>) => void;
  setStarting: (tabId: string, starting: boolean) => void;
  patchUi: (tabId: string, patch: Partial<TabDebugUi>) => void;
  setChildren: (tabId: string, reference: string, children: DebugVariable[] | null) => void;
  setKeymap: (keymap: DebugKeymap) => void;
  setInlineValues: (on: boolean) => void;
  patchStartOptions: (patch: Partial<DebugOptions>) => void;
  forgetTab: (tabId: string) => void;
}

const PREFS_KEY = 'dbstudio.debugger.prefs';

interface Prefs {
  inlineValues: boolean;
  startOptions: DebugOptions;
}

function readPrefs(): Prefs {
  const fallback: Prefs = { inlineValues: true, startOptions: { ...DEFAULT_DEBUG_OPTIONS } };
  try {
    const raw = localStorage.getItem(PREFS_KEY);
    if (!raw) return fallback;
    const parsed = JSON.parse(raw) as Partial<Prefs>;
    return {
      inlineValues: parsed.inlineValues ?? true,
      startOptions: { ...DEFAULT_DEBUG_OPTIONS, ...parsed.startOptions },
    };
  } catch {
    return fallback;
  }
}

function writePrefs(prefs: Prefs): void {
  try {
    localStorage.setItem(PREFS_KEY, JSON.stringify(prefs));
  } catch {
    // storage unavailable: preferences just are not kept
  }
}

const initialPrefs = readPrefs();

export const useDebugStore = create<DebugStoreState>((set, get) => ({
  sessions: {},
  starting: {},
  scopes: {},
  watches: {},
  muted: {},
  ui: {},
  children: {},
  keymap: 'jetBrains',
  inlineValues: initialPrefs.inlineValues,
  startOptions: initialPrefs.startOptions,

  ensureScope: (key, info) =>
    set((s) =>
      s.scopes[key] ? s : { scopes: { ...s.scopes, [key]: { ...info, breakpoints: [] } } },
    ),
  setBreakpoints: (key, breakpoints) =>
    set((s) => {
      const scope = s.scopes[key];
      if (!scope) return s;
      return { scopes: { ...s.scopes, [key]: { ...scope, breakpoints } } };
    }),
  setWatches: (tabId, watches) => set((s) => ({ watches: { ...s.watches, [tabId]: watches } })),
  setMuted: (tabId, muted) => set((s) => ({ muted: { ...s.muted, [tabId]: muted } })),
  setSession: (tabId, session) =>
    set((s) => {
      const sessions = { ...s.sessions };
      if (session) sessions[tabId] = session;
      else delete sessions[tabId];
      const children = { ...s.children };
      delete children[tabId];
      return { sessions, children };
    }),
  patchSession: (tabId, patch) =>
    set((s) => {
      const current = s.sessions[tabId];
      return current ? { sessions: { ...s.sessions, [tabId]: { ...current, ...patch } } } : s;
    }),
  setStarting: (tabId, starting) =>
    set((s) => ({ starting: { ...s.starting, [tabId]: starting } })),
  patchUi: (tabId, patch) =>
    set((s) => ({ ui: { ...s.ui, [tabId]: { ...(s.ui[tabId] ?? DEFAULT_UI), ...patch } } })),
  setChildren: (tabId, reference, children) =>
    set((s) => {
      const tab = { ...(s.children[tabId] ?? {}) };
      if (children) tab[reference] = children;
      else delete tab[reference];
      return { children: { ...s.children, [tabId]: tab } };
    }),
  setKeymap: (keymap) => set({ keymap }),
  setInlineValues: (inlineValues) => {
    set({ inlineValues });
    writePrefs({ inlineValues, startOptions: get().startOptions });
  },
  patchStartOptions: (patch) => {
    const startOptions = { ...get().startOptions, ...patch };
    set({ startOptions });
    writePrefs({ inlineValues: get().inlineValues, startOptions });
  },
  forgetTab: (tabId) =>
    set((s) => {
      const sessions = { ...s.sessions };
      delete sessions[tabId];
      const watches = { ...s.watches };
      delete watches[tabId];
      const muted = { ...s.muted };
      delete muted[tabId];
      const ui = { ...s.ui };
      delete ui[tabId];
      const children = { ...s.children };
      delete children[tabId];
      const starting = { ...s.starting };
      delete starting[tabId];
      const scopes = { ...s.scopes };
      delete scopes[scriptScope(tabId)];
      return { sessions, watches, muted, ui, children, starting, scopes };
    }),
}));

export const sessionOf = (tabId: string): DebugSession | undefined =>
  useDebugStore.getState().sessions[tabId];

export const uiOf = (tabId: string): TabDebugUi => useDebugStore.getState().ui[tabId] ?? DEFAULT_UI;

/** Whether the session is still alive (not terminated). */
export const isLive = (session: DebugSession | undefined): boolean =>
  !!session && session.state.status.kind !== 'terminated';

/** The tab whose live session has a frame in the routine, if any. */
export function tabOfRoutine(signature: string): string | null {
  for (const [tabId, session] of Object.entries(useDebugStore.getState().sessions)) {
    if (
      session.state.frames.some(
        (f) => f.source.kind === 'routine' && f.source.routine.signature === signature,
      )
    )
      return tabId;
  }
  return null;
}

/** Test helper. */
export function resetDebugStore(): void {
  useDebugStore.setState({
    sessions: {},
    starting: {},
    scopes: {},
    watches: {},
    muted: {},
    ui: {},
    children: {},
    keymap: 'jetBrains',
    inlineValues: true,
    startOptions: { ...DEFAULT_DEBUG_OPTIONS },
  });
}
