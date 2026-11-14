// Side effects of the debugger: IPC commands, events, breakpoint/watch persistence, the tab
// cleanup hook. Components and keymaps call these; state lives in `debugStore.ts`.

import { toast } from '@/components/ui/toastStore';
import { askConfirmation } from '@/features/safety/confirmStore';
import { ensureLease } from '@/features/editor/editorRunner';
import { getTabData, onEditorTabRemoved } from '@/features/editor/editorStore';
import { toParamValues } from '@/features/editor/params';
import { registerPersistHooks, notifyDebugStateChanged } from '@/features/editor/persistHooks';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { utf16ToCharOffset } from '@/lib/text';
import {
  type Breakpoint,
  type DebugAction,
  type DebugEndAction,
  type DebugOptions,
  type DebugOutput,
  type DebugOutputEvent,
  type DebugState,
  type DebugVariable,
  type EvalResult,
  type IpcError,
  type RoutineBreakpoint,
  type RoutineRef,
  type TabDebugState,
  type WatchSpec,
} from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { editorTabId, useTabsStore, type TabItem } from '@/stores/tabs';
import { cursorLine, docText, insertAtCursor, revealDocLine, viewOf } from './cm/viewRegistry';
import type { OutputItem } from './consoleModel';
import {
  OUTPUT_LIMIT,
  isLive,
  routineScope,
  scriptScope,
  sessionOf,
  tabOfRoutine,
  useDebugStore,
  type DebugSession,
} from './debugStore';
import { useDebugUi } from './debugUiStore';
import type { DebugCommand } from './keymap';

const t = (key: string, options?: Record<string, unknown>): string =>
  i18n.t(key, { ns: 'debugger', ...options });

const errorOf = (e: unknown): IpcError =>
  typeof e === 'object' && e !== null && 'kind' in e && 'message' in e
    ? (e as IpcError)
    : { kind: 'other', message: String(e) };

function fail(e: unknown): void {
  toast.error(errorOf(e).message);
}

// -- events ---------------------------------------------------------------------------------

let outputCounter = 0;
const PENDING_LIMIT = 200;
/** Events that arrived before `debug_start` resolved (the session is not bound yet). */
const pendingStates = new Map<string, DebugState[]>();
const pendingOutputs = new Map<string, DebugOutputEvent[]>();
let eventsReady: Promise<void> | null = null;
const unsubscribers: Array<() => void> = [];

const tabOfDebugId = (debugId: string): string | null => {
  for (const [tabId, s] of Object.entries(useDebugStore.getState().sessions)) {
    if (s.debugId === debugId) return tabId;
  }
  return null;
};

function push<T>(map: Map<string, T[]>, key: string, item: T): void {
  const list = map.get(key) ?? [];
  list.push(item);
  if (list.length > PENDING_LIMIT) list.shift();
  map.set(key, list);
}

/** Subscribe to `debug://state` and `debug://output` once. */
export function ensureDebugEvents(): Promise<void> {
  eventsReady ??= (async () => {
    unsubscribers.push(
      await ipc.onEvent<DebugState>('debug://state', (state) => {
        const tabId = tabOfDebugId(state.debugId);
        if (tabId) applyState(tabId, state, 'event');
        else push(pendingStates, state.debugId, state);
      }),
      await ipc.onEvent<DebugOutputEvent>('debug://output', (event) => {
        const tabId = tabOfDebugId(event.debugId);
        if (tabId) applyOutput(tabId, event);
        else push(pendingOutputs, event.debugId, event);
      }),
    );
  })();
  return eventsReady;
}

/** Test helper: drop subscriptions and buffers. */
export function resetDebugActions(): void {
  for (const off of unsubscribers.splice(0)) off();
  eventsReady = null;
  pendingStates.clear();
  pendingOutputs.clear();
}

function appendOutput(session: DebugSession, seq: number, output: DebugOutput): OutputItem[] {
  const next = session.outputs.concat({ id: ++outputCounter, seq, output });
  return next.length > OUTPUT_LIMIT ? next.slice(next.length - OUTPUT_LIMIT) : next;
}

function applyOutput(tabId: string, event: DebugOutputEvent): void {
  const session = sessionOf(tabId);
  if (!session || event.seq <= session.lastOutputSeq) return;
  const gap = session.lastSeq > 0 && event.seq > session.lastSeq + 1;
  useDebugStore.getState().patchSession(tabId, {
    outputs: appendOutput(session, event.seq, event.output),
    lastOutputSeq: event.seq,
    lastSeq: Math.max(session.lastSeq, event.seq),
  });
  if (gap) void resync(tabId);
}

/** A gap in `seq`: events were lost, ask the backend for the whole state. */
async function resync(tabId: string): Promise<void> {
  const session = sessionOf(tabId);
  if (!session) return;
  try {
    const state = await ipc.debugSnapshot(session.debugId);
    applyState(tabId, state, 'sync');
  } catch (e) {
    fail(e);
  }
}

/**
 * Take a state snapshot when it is newer than the one shown. Only events are checked for gaps
 * (a command result may legitimately run ahead of the events it caused).
 */
export function applyState(
  tabId: string,
  state: DebugState,
  from: 'event' | 'result' | 'sync' = 'result',
): void {
  const session = sessionOf(tabId);
  if (!session || session.debugId !== state.debugId) return;
  if (from !== 'sync' && state.seq <= session.state.seq) return;
  const gap = from === 'event' && session.lastSeq > 0 && state.seq > session.lastSeq + 1;
  useDebugStore.getState().patchSession(tabId, {
    state,
    lastSeq: from === 'result' ? session.lastSeq : Math.max(session.lastSeq, state.seq),
  });
  afterState(tabId, state, session.state);
  if (gap) void resync(tabId);
}

function afterState(tabId: string, state: DebugState, previous: DebugState | null): void {
  const status = state.status;
  if (status.kind === 'paused') {
    const was = previous?.status.kind === 'paused' ? previous.status.reason.kind : null;
    if (status.reason.kind === 'awaitingEnd' && was !== 'awaitingEnd') {
      useDebugUi.getState().openDialog(tabId, { kind: 'end', then: 'stop' });
    }
    followStop(tabId, state, previous, status.reason.kind);
    useDebugStore.getState().patchUi(tabId, { bottom: 'debug' });
  }
}

/** Where a paused state stands: the routine signature, or null for the script. */
function stopSignature(state: DebugState | null): string | null {
  if (!state || state.status.kind !== 'paused') return null;
  const source = state.position?.source ?? state.frames[state.selectedFrame]?.source;
  return source?.kind === 'routine' ? source.routine.signature : null;
}

/**
 * JetBrains-style follow: a stop in a routine opens its source tab; the focus moves there when
 * the routine is new or the stop was not a plain step, and only from the session's own tabs.
 * Stepping inside the same routine never switches tabs.
 */
function followStop(
  tabId: string,
  state: DebugState,
  previous: DebugState | null,
  reason: string,
): void {
  const sig = stopSignature(state);
  const before = stopSignature(previous);
  const source = state.position?.source ?? state.frames[state.selectedFrame]?.source;
  const tabs = useTabsStore.getState();
  const active = tabs.tabs.find((x) => x.id === tabs.activeId);
  const own = (x: typeof active): boolean =>
    x?.kind === 'editor'
      ? x.payload.tabStateId === tabId
      : x?.kind === 'debugSource' &&
        (tabOfRoutine(x.payload.signature) === tabId ||
          x.payload.signature === sig ||
          x.payload.signature === before);
  const mine = own(active);
  if (sig && source?.kind === 'routine') {
    const focus = mine && (sig !== before || reason !== 'step');
    const had = openRoutineTab(tabId, source.routine, focus);
    if (focus && had) tabs.activate(had);
    return;
  }
  // Back in the script (step out / finish the routine): bring its editor back.
  if (!sig && before && active?.kind === 'debugSource' && mine) tabs.activate(editorTabId(tabId));
}

/**
 * Open (or reuse) the read-only source tab of a routine the debugger stopped in. `activate`
 * false opens it in the background. Returns the tab id (null when it cannot be opened).
 */
export function openRoutineTab(tabId: string, routine: RoutineRef, activate = true): string | null {
  const data = getTabData(tabId);
  const editorTab = useTabsStore
    .getState()
    .tabs.find((x) => x.kind === 'editor' && x.payload.tabStateId === tabId);
  const sessionId = data?.leaseSessionId ?? (editorTab?.payload?.sessionId as string | undefined);
  const connectionId = editorTab?.kind === 'editor' ? editorTab.payload.connectionId : null;
  if (!sessionId || !connectionId) return null;
  const id = `debugSource:${sessionId}:${routine.signature}`;
  const tabs = useTabsStore.getState();
  if (tabs.tabs.some((x) => x.id === id)) {
    if (activate) tabs.activate(id);
    return id;
  }
  const name = (routine.signature.split('(')[0] ?? routine.signature).split('.').pop();
  const item: TabItem = {
    id,
    kind: 'debugSource',
    title: `${name}() [debug]`,
    closable: true,
    payload: { sessionId, connectionId, signature: routine.signature },
  };
  if (activate) tabs.openTab(item);
  else useTabsStore.setState((s) => ({ tabs: [...s.tabs, item] }));
  return id;
}

// -- scopes, breakpoints --------------------------------------------------------------------

/** Register the script scope of an editor tab (idempotent). */
export function ensureScriptScope(tabId: string): void {
  useDebugStore.getState().ensureScope(scriptScope(tabId), {
    source: { kind: 'script' },
    tabId,
  });
}

/** Register the scope of a routine source and load its saved breakpoints once. */
export async function ensureRoutineScope(
  connectionId: string,
  routine: RoutineRef,
  sourceHash?: string | null,
): Promise<string> {
  const key = routineScope(connectionId, routine.signature);
  const store = useDebugStore.getState();
  const fresh = !store.scopes[key];
  store.ensureScope(key, {
    source: { kind: 'routine', routine },
    connectionId,
    routine,
    sourceHash,
  });
  if (fresh) {
    try {
      const saved = await ipc.routineBreakpointsGet(connectionId);
      for (const rb of saved) {
        const k = routineScope(connectionId, rb.routine.signature);
        useDebugStore.getState().ensureScope(k, {
          source: { kind: 'routine', routine: rb.routine },
          connectionId,
          routine: rb.routine,
          sourceHash: rb.sourceHash,
        });
        const current = useDebugStore.getState().scopes[k]!.breakpoints;
        if (!current.some((b) => b.id === rb.breakpoint.id)) {
          useDebugStore.getState().setBreakpoints(k, [...current, rb.breakpoint]);
        }
      }
    } catch (e) {
      fail(e);
    }
  }
  return key;
}

const newId = (): string => crypto.randomUUID();

function routineList(connectionId: string): RoutineBreakpoint[] {
  const out: RoutineBreakpoint[] = [];
  for (const scope of Object.values(useDebugStore.getState().scopes)) {
    if (scope.connectionId !== connectionId || !scope.routine) continue;
    for (const breakpoint of scope.breakpoints) {
      out.push({ routine: scope.routine, sourceHash: scope.sourceHash ?? null, breakpoint });
    }
  }
  return out;
}

/** The session a scope's breakpoints must be pushed to, if any. */
function liveSessionForScope(key: string): [string, DebugSession] | null {
  const scope = useDebugStore.getState().scopes[key];
  if (!scope) return null;
  if (scope.tabId) {
    const session = sessionOf(scope.tabId);
    return session && isLive(session) ? [scope.tabId, session] : null;
  }
  if (scope.routine) {
    const tab = tabOfRoutine(scope.routine.signature);
    const session = tab ? sessionOf(tab) : undefined;
    if (tab && session && isLive(session)) return [tab, session];
  }
  return null;
}

async function pushBreakpoints(key: string): Promise<void> {
  const scope = useDebugStore.getState().scopes[key];
  if (!scope) return;
  if (scope.connectionId) {
    ipc.routineBreakpointsSet(scope.connectionId, routineList(scope.connectionId)).catch(fail);
  } else {
    notifyDebugStateChanged();
  }
  const live = liveSessionForScope(key);
  if (!live) return;
  const [tabId, session] = live;
  try {
    const statuses = await ipc.debugSetBreakpoints(
      session.debugId,
      scope.source,
      scope.breakpoints,
    );
    const current = sessionOf(tabId);
    if (!current || current.debugId !== session.debugId) return;
    const ids = new Set(scope.breakpoints.map((b) => b.id));
    const kept = current.state.breakpoints.filter((s) => !ids.has(s.id));
    const next: DebugState = { ...current.state, breakpoints: [...kept, ...statuses] };
    useDebugStore.getState().patchSession(tabId, { state: next });
  } catch (e) {
    fail(e);
  }
}

function setScopeBreakpoints(key: string, list: Breakpoint[]): void {
  useDebugStore.getState().setBreakpoints(key, list);
  void pushBreakpoints(key);
}

const breakpointsOf = (key: string): Breakpoint[] =>
  useDebugStore.getState().scopes[key]?.breakpoints ?? [];

/**
 * Click on a gutter line: toggle the breakpoint. With `disableOnly` (Alt+click) an existing one
 * is enabled/disabled in place and a missing one is created disabled.
 */
export function toggleBreakpoint(key: string, line: number, disableOnly = false): void {
  const list = breakpointsOf(key);
  const existing = list.find((b) => b.line === line);
  if (existing) {
    if (disableOnly) {
      setScopeBreakpoints(
        key,
        list.map((b) => (b.id === existing.id ? { ...b, enabled: !b.enabled } : b)),
      );
    } else {
      setScopeBreakpoints(
        key,
        list.filter((b) => b.id !== existing.id),
      );
    }
    return;
  }
  const created: Breakpoint = { id: newId(), line, enabled: !disableOnly, suspend: true };
  setScopeBreakpoints(
    key,
    [...list, created].sort((a, b) => a.line - b.line),
  );
}

export function updateBreakpoint(key: string, id: string, patch: Partial<Breakpoint>): void {
  setScopeBreakpoints(
    key,
    breakpointsOf(key).map((b) => (b.id === id ? { ...b, ...patch } : b)),
  );
}

export function removeBreakpoint(key: string, id: string): void {
  setScopeBreakpoints(
    key,
    breakpointsOf(key).filter((b) => b.id !== id),
  );
}

export function removeAllBreakpoints(keys: string[]): void {
  for (const key of keys) if (breakpointsOf(key).length > 0) setScopeBreakpoints(key, []);
}

/** The editor moved breakpoints while text was typed: keep the new lines (no backend push). */
export function syncBreakpointLines(key: string, lines: Array<{ id: string; line: number }>): void {
  const list = breakpointsOf(key);
  const byId = new Map(lines.map((l) => [l.id, l.line]));
  const seen = new Set<number>();
  const next: Breakpoint[] = [];
  for (const b of list) {
    const line = byId.get(b.id);
    if (line === undefined) continue; // its line was deleted
    if (seen.has(line)) continue; // two merged onto one line
    seen.add(line);
    next.push(line === b.line ? b : { ...b, line });
  }
  const same = next.length === list.length && next.every((b, i) => b === list[i]);
  if (same) return;
  useDebugStore.getState().setBreakpoints(key, next);
  void pushBreakpoints(key);
}

// -- watches, mute --------------------------------------------------------------------------

export const watchesOf = (tabId: string): WatchSpec[] =>
  useDebugStore.getState().watches[tabId] ?? [];

async function commitWatches(tabId: string, watches: WatchSpec[]): Promise<void> {
  useDebugStore.getState().setWatches(tabId, watches);
  notifyDebugStateChanged();
  const session = sessionOf(tabId);
  if (!session || !isLive(session)) return;
  try {
    const results = await ipc.debugWatchesSet(session.debugId, watches);
    const current = sessionOf(tabId);
    if (current)
      useDebugStore
        .getState()
        .patchSession(tabId, { state: { ...current.state, watches: results } });
  } catch (e) {
    fail(e);
  }
}

export function addWatch(tabId: string, expr: string): Promise<void> {
  const text = expr.trim();
  if (!text) return Promise.resolve();
  return commitWatches(tabId, [...watchesOf(tabId), { id: newId(), expr: text }]);
}

export function removeWatch(tabId: string, id: string): Promise<void> {
  return commitWatches(
    tabId,
    watchesOf(tabId).filter((w) => w.id !== id),
  );
}

export function editWatch(tabId: string, id: string, expr: string): Promise<void> {
  const text = expr.trim();
  if (!text) return removeWatch(tabId, id);
  return commitWatches(
    tabId,
    watchesOf(tabId).map((w) => (w.id === id ? { ...w, expr: text } : w)),
  );
}

export function moveWatch(tabId: string, fromId: string, toId: string): Promise<void> {
  const list = [...watchesOf(tabId)];
  const from = list.findIndex((w) => w.id === fromId);
  const to = list.findIndex((w) => w.id === toId);
  if (from < 0 || to < 0 || from === to) return Promise.resolve();
  const [moved] = list.splice(from, 1);
  list.splice(to, 0, moved!);
  return commitWatches(tabId, list);
}

export async function setMuted(tabId: string, muted: boolean): Promise<void> {
  useDebugStore.getState().setMuted(tabId, muted);
  notifyDebugStateChanged();
  const session = sessionOf(tabId);
  if (!session || !isLive(session)) return;
  try {
    applyState(tabId, await ipc.debugSetMuted(session.debugId, muted));
  } catch (e) {
    fail(e);
  }
}

// -- persistence of tabs --------------------------------------------------------------------

registerPersistHooks({
  debugStateOf: (tabId): TabDebugState | null => {
    const s = useDebugStore.getState();
    const breakpoints = s.scopes[scriptScope(tabId)]?.breakpoints ?? [];
    const watches = s.watches[tabId] ?? [];
    const muted = s.muted[tabId] ?? false;
    if (breakpoints.length === 0 && watches.length === 0 && !muted) return null;
    return { breakpoints, watches, muted };
  },
  applyDebugState: (tabId, state) => {
    ensureScriptScope(tabId);
    const s = useDebugStore.getState();
    s.setBreakpoints(scriptScope(tabId), state.breakpoints);
    s.setWatches(tabId, state.watches);
    s.setMuted(tabId, state.muted);
  },
});

// A closed tab ends its session: the owned transaction is rolled back (nobody can be asked).
onEditorTabRemoved((tabId) => {
  const session = sessionOf(tabId);
  if (session && isLive(session)) {
    ipc.debugStop(session.debugId, 'rollback').catch(() => undefined);
    if (session.state.transaction.owned) toast.info(t('debugger:toast.closedRollback'));
  }
  useDebugStore.getState().forgetTab(tabId);
});

// -- start / stop ---------------------------------------------------------------------------

/** Current text of the tab's editor (the view has the freshest copy). */
function scriptText(tabId: string): string {
  return docText(scriptScope(tabId)) ?? getTabData(tabId)?.text ?? '';
}

export async function startDebug(
  tabId: string,
  sessionId: string,
  override?: Partial<DebugOptions>,
): Promise<void> {
  const store = useDebugStore.getState();
  if (store.starting[tabId] || (sessionOf(tabId) && isLive(sessionOf(tabId)))) return;
  store.setStarting(tabId, true);
  try {
    const data = getTabData(tabId);
    if (!data) throw { kind: 'notFound', message: `editor ${tabId}` };
    ensureScriptScope(tabId);
    await ensureDebugEvents();
    if (sessionOf(tabId)) closeToolWindow(tabId);
    const leaseId = await ensureLease(tabId, sessionId);
    const text = scriptText(tabId);
    const view = viewOf(scriptScope(tabId));
    const sel = view?.state.selection.main ?? data.selection;
    const hasSel = sel.from !== sel.to;
    const options: DebugOptions = { ...useDebugStore.getState().startOptions, ...override };
    const state = await ipc.debugStart({
      leaseId,
      text,
      range: hasSel
        ? { start: utf16ToCharOffset(text, sel.from), end: utf16ToCharOffset(text, sel.to) }
        : null,
      params: toParamValues(data.params),
      breakpoints: breakpointsOf(scriptScope(tabId)),
      routineBreakpoints: routineList(editorConnectionId(tabId) ?? ''),
      watches: watchesOf(tabId),
      options,
      muted: useDebugStore.getState().muted[tabId] ?? false,
    });
    attach(tabId, state, options);
  } catch (e) {
    fail(e);
  } finally {
    useDebugStore.getState().setStarting(tabId, false);
  }
}

export function editorConnectionId(tabId: string): string | null {
  const tab = useTabsStore
    .getState()
    .tabs.find((x) => x.kind === 'editor' && x.payload.tabStateId === tabId);
  return tab?.kind === 'editor' ? tab.payload.connectionId : null;
}

/** Bind a fresh session to the tab and replay what arrived before it was bound. */
function attach(tabId: string, state: DebugState, options: DebugOptions): void {
  const store = useDebugStore.getState();
  store.setSession(tabId, {
    debugId: state.debugId,
    state,
    outputs: [],
    lastSeq: state.seq,
    lastOutputSeq: 0,
    busy: false,
    options,
    lastAction: null,
    lastStepMs: null,
  });
  store.patchUi(tabId, { bottom: 'debug', panel: 'debugger' });
  const outs = pendingOutputs.get(state.debugId) ?? [];
  const states = pendingStates.get(state.debugId) ?? [];
  pendingOutputs.delete(state.debugId);
  pendingStates.delete(state.debugId);
  for (const o of outs) applyOutput(tabId, o);
  for (const s of states) applyState(tabId, s, 'event');
  afterState(tabId, sessionOf(tabId)?.state ?? state, null);
}

export function closeToolWindow(tabId: string): void {
  const session = sessionOf(tabId);
  if (session && isLive(session)) return;
  useDebugStore.getState().setSession(tabId, null);
  useDebugStore.getState().patchUi(tabId, { bottom: 'results' });
  useDebugUi.getState().closeDialog(tabId);
}

/** End actions need a question when the debugger owns the transaction and the choice is `ask`. */
const needsEndQuestion = (session: DebugSession): boolean =>
  session.state.transaction.owned && session.options.endAction === 'ask';

export async function stopDebug(tabId: string, end: DebugEndAction | null = null): Promise<void> {
  const session = sessionOf(tabId);
  if (!session || !isLive(session)) return;
  try {
    await ipc.debugStop(session.debugId, end);
    // The state after a stop depends on the end action (awaitingEnd, terminated): ask for it.
    applyState(tabId, await ipc.debugSnapshot(session.debugId), 'sync');
  } catch (e) {
    fail(e);
  }
}

/** Stop button / Cmd+F2: asks Commit or Rollback when the debugger owns the transaction. */
export function requestStop(tabId: string): void {
  const session = sessionOf(tabId);
  if (!session) return;
  if (!isLive(session)) return closeToolWindow(tabId);
  if (needsEndQuestion(session)) {
    useDebugUi.getState().openDialog(tabId, { kind: 'end', then: 'stop' });
  } else {
    void stopDebug(tabId);
  }
}

export async function restartDebug(
  tabId: string,
  end: DebugEndAction | null = null,
): Promise<void> {
  const session = sessionOf(tabId);
  const data = getTabData(tabId);
  if (!session || !data) return;
  if (!isLive(session)) {
    closeToolWindow(tabId);
    const sessionId = liveSessionId(tabId);
    if (sessionId) await startDebug(tabId, sessionId);
    return;
  }
  try {
    const state = await ipc.debugRestart(session.debugId, scriptText(tabId), end);
    attach(tabId, state, session.options);
  } catch (e) {
    fail(e);
  }
}

export function requestRestart(tabId: string): void {
  const session = sessionOf(tabId);
  if (!session) return;
  if (isLive(session) && needsEndQuestion(session)) {
    useDebugUi.getState().openDialog(tabId, { kind: 'end', then: 'restart' });
  } else {
    void restartDebug(tabId);
  }
}

/** Answer of the Commit / Rollback / Keep dialog. */
export function answerEnd(tabId: string, end: DebugEndAction): void {
  const dialog = useDebugUi.getState().dialogs[tabId];
  useDebugUi.getState().closeDialog(tabId);
  if (dialog?.kind === 'end' && dialog.then === 'restart') void restartDebug(tabId, end);
  else void stopDebug(tabId, end);
}

// -- stepping -------------------------------------------------------------------------------

export async function step(tabId: string, action: DebugAction): Promise<void> {
  const session = sessionOf(tabId);
  if (!session || session.busy) return;
  useDebugStore.getState().patchSession(tabId, { busy: true, lastAction: action });
  const started = performance.now();
  try {
    const state = await ipc.debugStep(session.debugId, action);
    applyState(tabId, state);
    useDebugStore
      .getState()
      .patchSession(tabId, { lastStepMs: Math.round(performance.now() - started) });
  } catch (e) {
    fail(e);
  } finally {
    useDebugStore.getState().patchSession(tabId, { busy: false });
  }
}

/** Retry the paused statement with the user's typed confirmation (production / destructive). */
export async function confirmPending(tabId: string): Promise<void> {
  const session = sessionOf(tabId);
  const status = session?.state.status;
  if (!session || status?.kind !== 'paused' || status.reason.kind !== 'confirmation') return;
  try {
    const grant = await askConfirmation(status.reason.request);
    const state = await ipc.debugStep(
      session.debugId,
      session.lastAction ?? { kind: 'stepOver' },
      grant,
    );
    applyState(tabId, state);
  } catch (e) {
    if (errorOf(e).kind !== 'cancelled') fail(e);
  }
}

export interface CommandContext {
  /** Scope of the editor the key was pressed in (default: the tab's script). */
  scopeKey?: string;
}

/** Whether `command` can run now (buttons are disabled with it). */
export function commandEnabled(tabId: string, command: DebugCommand): boolean {
  const session = sessionOf(tabId);
  if (command === 'debug') return !session || !isLive(session);
  if (command === 'toggleBreakpoint' || command === 'viewBreakpoints') return true;
  if (command === 'muteBreakpoints') return true;
  if (!session) return false;
  const { status, caps } = session.state;
  if (command === 'rerun') return true;
  if (status.kind === 'terminated') return false;
  if (command === 'stop') return true;
  if (status.kind === 'running') return command === 'pause' && caps.pauseRunning;
  if (status.kind !== 'paused' || session.busy) return false;
  switch (command) {
    case 'pause':
      return false;
    case 'stepInto':
    case 'forceStepInto':
      return caps.stepInto;
    case 'smartStepInto':
      return caps.stepInto && (session.state.stepIntoTargets?.length ?? 0) > 0;
    case 'stepOut':
      return caps.stepOut;
    default:
      return true;
  }
}

/** Run a debugger command for a tab (toolbar, keymap, menus). */
export function runCommand(tabId: string, command: DebugCommand, ctx: CommandContext = {}): void {
  const scopeKey = ctx.scopeKey ?? scriptScope(tabId);
  const session = sessionOf(tabId);
  switch (command) {
    case 'debug': {
      const sessionId = liveSessionId(tabId);
      if (sessionId) void startDebug(tabId, sessionId);
      else toast.error(t('debugger:toast.noSession'));
      return;
    }
    case 'toggleBreakpoint': {
      const line = cursorLineOf(scopeKey, tabId);
      if (line !== null) toggleBreakpoint(scopeKey, line);
      return;
    }
    case 'viewBreakpoints':
      useDebugUi.getState().openDialog(tabId, { kind: 'breakpoints' });
      return;
    case 'muteBreakpoints':
      void setMuted(tabId, !(useDebugStore.getState().muted[tabId] ?? false));
      return;
    default:
  }
  if (!session || !commandEnabled(tabId, command)) return;
  switch (command) {
    case 'resume':
      return void step(tabId, { kind: 'resume' });
    case 'pause':
      return void step(tabId, { kind: 'pause' });
    case 'stepOver':
    case 'stepInto':
    case 'forceStepInto':
    case 'stepOut':
      return void step(tabId, { kind: command });
    case 'smartStepInto': {
      const targets = session.state.stepIntoTargets ?? [];
      if (targets.length === 1)
        void step(tabId, { kind: 'smartStepInto', signature: targets[0]!.signature });
      else useDebugUi.getState().openDialog(tabId, { kind: 'smartStepInto' });
      return;
    }
    case 'runToCursor':
    case 'forceRunToCursor': {
      const scope = useDebugStore.getState().scopes[scopeKey];
      const line = cursorLineOf(scopeKey, tabId);
      if (!scope || line === null) return;
      return void step(tabId, {
        kind: 'runToCursor',
        source: scope.source,
        line,
        force: command === 'forceRunToCursor',
      });
    }
    case 'stop':
      return requestStop(tabId);
    case 'rerun':
      return requestRestart(tabId);
    case 'evaluate':
      return useDebugUi
        .getState()
        .openDialog(tabId, { kind: 'evaluate', expr: selectedExpr(scopeKey) });
    case 'showExecutionPoint':
      return showExecutionPoint(tabId);
  }
}

/**
 * The session the editor runs on now. A restored tab keeps the id of a session that no longer
 * exists in its lease, so the connection's live session wins over `leaseSessionId`.
 */
function liveSessionId(tabId: string): string | null {
  const tab = useTabsStore
    .getState()
    .tabs.find((x) => x.kind === 'editor' && x.payload.tabStateId === tabId);
  if (tab?.kind !== 'editor') return null;
  const connectionId = tab.payload.connectionId;
  const live = connectionId
    ? useSessionsStore.getState().sessionsByConnection[connectionId]
    : undefined;
  return live?.sessionId ?? tab.payload.sessionId ?? null;
}

/** Cursor line of an editor: the mounted view, else the persisted selection of the tab. */
function cursorLineOf(scopeKey: string, tabId: string): number | null {
  const live = cursorLine(scopeKey);
  if (live !== null) return live;
  const data = getTabData(tabId);
  if (!data || scopeKey !== scriptScope(tabId)) return null;
  return data.text.slice(0, data.selection.to).split('\n').length;
}

/** Selection or word under the cursor of the editor, for the Evaluate dialog. */
function selectedExpr(scopeKey: string): string | undefined {
  const view = viewOf(scopeKey);
  if (!view) return undefined;
  const { from, to, head } = view.state.selection.main;
  if (from !== to) return view.state.sliceDoc(from, to);
  const word = view.state.wordAt(head);
  return word ? view.state.sliceDoc(word.from, word.to) : undefined;
}

export function showExecutionPoint(tabId: string): void {
  const session = sessionOf(tabId);
  const frame = session?.state.frames[session.state.selectedFrame];
  if (!session || !frame) return;
  if (frame.source.kind === 'routine') {
    openRoutineTab(tabId, frame.source.routine);
    const connectionId = editorConnectionId(tabId);
    if (connectionId)
      revealDocLine(routineScope(connectionId, frame.source.routine.signature), frame.line);
  } else {
    useDebugStore.getState().patchUi(tabId, { bottom: 'debug' });
    revealDocLine(scriptScope(tabId), frame.line);
  }
}

// -- frames, variables, evaluation ---------------------------------------------------------

export async function selectFrame(tabId: string, frame: number): Promise<void> {
  const session = sessionOf(tabId);
  if (!session || !isLive(session)) return;
  try {
    applyState(tabId, await ipc.debugSelectFrame(session.debugId, frame));
  } catch (e) {
    fail(e);
  }
}

export async function loadChildren(tabId: string, reference: string): Promise<DebugVariable[]> {
  const session = sessionOf(tabId);
  if (!session) return [];
  const children = await ipc.debugVariableChildren(session.debugId, reference);
  useDebugStore.getState().setChildren(tabId, reference, children);
  return children;
}

export async function setVariable(
  tabId: string,
  reference: string,
  valueText: string,
): Promise<DebugVariable | null> {
  const session = sessionOf(tabId);
  if (!session || !isLive(session)) return null;
  try {
    const updated = await ipc.debugSetVariable(session.debugId, reference, valueText);
    // Watches and inline values may depend on it: take a fresh snapshot.
    void resync(tabId);
    return updated;
  } catch (e) {
    fail(e);
    return null;
  }
}

/** Evaluate in the selected frame; `echo` also writes the result to the console. */
export async function evaluateExpression(
  tabId: string,
  expr: string,
  echo = false,
): Promise<EvalResult | null> {
  const session = sessionOf(tabId);
  if (!session || !isLive(session)) return null;
  const before = outputCounter;
  try {
    const result = await ipc.debugEvaluate(
      session.debugId,
      expr,
      session.state.selectedFrame,
      echo,
    );
    if (echo) {
      const current = sessionOf(tabId);
      const echoed = current?.outputs.some(
        (o) => o.id > before && o.output.kind === 'evaluated' && o.output.expr === expr,
      );
      // A backend that echoed already sent the entry; otherwise show the result locally.
      if (current && !echoed) {
        useDebugStore.getState().patchSession(tabId, {
          outputs: appendOutput(current, current.lastSeq, { kind: 'evaluated', expr, result }),
        });
      }
    }
    return result;
  } catch (e) {
    fail(e);
    return null;
  }
}

/** Where the user typed a command at the Console prompt (history is per tab). */
const consoleHistory = new Map<string, string[]>();
export const historyOf = (tabId: string): string[] => consoleHistory.get(tabId) ?? [];
export function pushHistory(tabId: string, expr: string): void {
  const list = historyOf(tabId).filter((x) => x !== expr);
  list.push(expr);
  consoleHistory.set(tabId, list.slice(-100));
}

/** Insert the "create extension" statement of a `needsExtension` probe into the editor. */
export function insertIntoScript(tabId: string, text: string): boolean {
  return insertAtCursor(scriptScope(tabId), text);
}

export async function loadSupport(tabId: string, sessionId: string): Promise<void> {
  try {
    const leaseId = await ensureLease(tabId, sessionId);
    const support = await ipc.debugSupport(leaseId);
    supportCache.set(tabId, support);
    supportListeners.forEach((f) => f());
  } catch {
    // the probe is optional: the menu simply has no routine hint
  }
}

import type { DebugSupport } from '@/lib/types';
const supportCache = new Map<string, DebugSupport>();
const supportListeners = new Set<() => void>();
export const supportOf = (tabId: string): DebugSupport | undefined => supportCache.get(tabId);
export function onSupportChanged(f: () => void): () => void {
  supportListeners.add(f);
  return () => supportListeners.delete(f);
}

// -- keymap preference (AppSettings.debugKeymap) --------------------------------------------

export async function loadKeymap(): Promise<void> {
  try {
    const settings = await ipc.settingsGet();
    useDebugStore.getState().setKeymap(settings.debugKeymap);
  } catch {
    // keep the default
  }
}

export async function saveKeymap(keymap: 'jetBrains' | 'vsCode'): Promise<void> {
  useDebugStore.getState().setKeymap(keymap);
  try {
    const settings = await ipc.settingsGet();
    await ipc.settingsSet({ ...settings, debugKeymap: keymap });
  } catch (e) {
    fail(e);
  }
}
