// Actions of the editors that talk to the backend: lease lifecycle, running text, cancelling,
// fetching more rows and transaction control. State goes into `editorStore`; results into a
// `ResultCollector` per run. Nothing here depends on React.

import { askConfirmation } from '@/features/safety/confirmStore';
import { guardedCall } from '@/features/safety/useGuardedCall';
import { useToastStore } from '@/components/ui/toastStore';
import i18n from '@/lib/i18n';
import { ipc, toIpcError } from '@/lib/ipc';
import {
  DEFAULT_SETTINGS,
  type AppSettings,
  type IpcError,
  type LeaseState,
  type ParamValue,
  type TxOptions,
  type Uuid,
} from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { registerTabGuard } from '@/stores/tabGuard';
import { editorTabId } from '@/stores/tabs';
import {
  getTabData,
  hasOpenTransaction,
  onEditorTabRemoved,
  patchTabData,
  useEditorStore,
  type EditorTabData,
  type RunSnapshot,
} from './editorStore';
import { locateError, problemMarks } from './errorMarks';
import {
  confirmDiscardResultEdits,
  dropResultEdits,
  hasResultEdits,
  onResultEditsChanged,
} from './resultEdit';
import { ResultCollector, type ResultGridModel } from './results';
import type { RunText } from './runScope';

/** Run state kept outside the store: collectors and in-flight lease opens. */
const collectors = new Map<string, ResultCollector>();
const opening = new Map<string, Promise<Uuid>>();
let runCounter = 0;

const PUBLISH_MS = 50;

function toast(error: unknown): void {
  useToastStore.getState().push('error', toIpcError(error).message);
}

// -- lease lifecycle ----------------------------------------------------------------------------

/** The lease of a tab, opened on first use and reopened after the session or connection changed. */
export async function ensureLease(tabId: string, sessionId: Uuid): Promise<Uuid> {
  const data = getTabData(tabId);
  if (!data) throw { kind: 'notFound', message: `editor ${tabId}` } satisfies IpcError;
  if (data.leaseId && data.leaseSessionId === sessionId && !data.leaseState?.broken) {
    return data.leaseId;
  }
  const pending = opening.get(tabId);
  if (pending) return pending;

  const open = (async () => {
    if (data.leaseId) await closeLease(tabId); // belongs to another session, or is broken
    patchTabData(tabId, { leaseStatus: 'opening', leaseError: null });
    try {
      const info = await ipc.leaseOpen(sessionId, {
        database: data.database,
        schema: data.schema,
        autoCommit: data.autoCommit,
      });
      if (!getTabData(tabId)) {
        // The tab closed while the lease was opening.
        await ipc.leaseClose(info.leaseId).catch(() => undefined);
        throw { kind: 'cancelled', message: 'editor closed' } satisfies IpcError;
      }
      patchTabData(tabId, {
        leaseId: info.leaseId,
        leaseSessionId: sessionId,
        leaseState: info.state,
        leaseStatus: 'ready',
        autoCommit: info.state.autoCommit,
      });
      return info.leaseId;
    } catch (e) {
      patchTabData(tabId, { leaseStatus: 'error', leaseError: toIpcError(e) });
      throw toIpcError(e);
    }
  })().finally(() => opening.delete(tabId));
  opening.set(tabId, open);
  return open;
}

/** Forget the lease and close it on the backend (rolls back an open transaction). */
export async function closeLease(tabId: string): Promise<void> {
  const data = getTabData(tabId);
  const leaseId = data?.leaseId;
  if (!data || !leaseId) return;
  patchTabData(tabId, {
    leaseId: null,
    leaseSessionId: null,
    leaseState: null,
    leaseStatus: 'none',
  });
  await ipc.leaseClose(leaseId).catch(() => undefined);
}

/** The backend lost the lease (session closed, connection dropped): keep the text, drop the rest. */
export function dropLease(tabId: string, reason: 'sessionClosed' | 'broken'): void {
  const data = getTabData(tabId);
  if (!data?.leaseId) return;
  const lost = hasOpenTransaction(data);
  patchTabData(tabId, {
    leaseId: reason === 'broken' ? data.leaseId : null,
    leaseSessionId: reason === 'broken' ? data.leaseSessionId : null,
    leaseState:
      reason === 'broken' && data.leaseState ? { ...data.leaseState, broken: true } : null,
    leaseStatus: 'none',
    transactionLost: lost,
  });
}

/** Apply a `lease://state` event. */
export function applyLeaseState(leaseId: Uuid, state: LeaseState): void {
  // The tab owning this lease, if any (leases of closed tabs are ignored).
  for (const tab of Object.values(useEditorStore.getState().tabs)) {
    if (tab.leaseId !== leaseId) continue;
    const lost = state.broken && hasOpenTransaction(tab);
    patchTabData(tab.id, { leaseState: state, transactionLost: tab.transactionLost || lost });
  }
}

// -- running ------------------------------------------------------------------------------------

function snapshotOf(
  runId: number,
  queryId: Uuid,
  input: RunOptions,
  status: RunSnapshot['status'],
): RunSnapshot {
  return {
    id: runId,
    status,
    queryId,
    text: input.text,
    baseChar: input.baseChar,
    params: input.params,
    results: [],
    messages: [],
    error: null,
    errorMark: null,
    startedAt: Date.now(),
    elapsedMs: null,
    canFetchMore: false,
    fetchingMore: false,
    docAtStart: '',
  };
}

function updateRun(tabId: string, runId: number, patch: Partial<RunSnapshot>): void {
  const run = getTabData(tabId)?.run;
  if (run?.id === runId) patchTabData(tabId, { run: { ...run, ...patch } });
}

async function readSettings(): Promise<AppSettings> {
  try {
    return await ipc.settingsGet();
  } catch {
    return DEFAULT_SETTINGS;
  }
}

/** Refresh the lease state from the backend (a failed statement may have aborted the transaction). */
async function refreshState(tabId: string): Promise<void> {
  const data = getTabData(tabId);
  if (!data?.leaseId) return;
  try {
    patchTabData(tabId, { leaseState: await ipc.leaseState(data.leaseId) });
  } catch (e) {
    if (toIpcError(e).kind === 'connection' && data.leaseState) {
      applyLeaseState(data.leaseId, { ...data.leaseState, broken: true, running: null });
    }
  }
}

function lastModel(collector: ResultCollector): ResultGridModel | null {
  for (let i = collector.results.length - 1; i >= 0; i--) {
    const m = collector.results[i]!.model;
    if (m) return m;
  }
  return null;
}

export interface RunOptions extends RunText {
  params?: ParamValue[];
}

/**
 * Run `input` on the tab's lease and stream the results into the tab's run snapshot. Resolves
 * to the final snapshot (`null` when a run is already in progress). Errors never reject: they
 * end up in the snapshot, with the marked position when the backend gave one.
 */
export async function runText(
  tabId: string,
  sessionId: Uuid,
  input: RunOptions,
): Promise<RunSnapshot | null> {
  const data = getTabData(tabId);
  if (!data || data.run?.status === 'running' || data.run?.fetchingMore) return null;
  // Running replaces the results, and with them the edits made in their grids.
  if (hasResultEdits(tabId)) {
    if (!confirmDiscardResultEdits(i18n.t('editor:results.edit.runConfirm'))) return null;
    dropResultEdits(tabId);
  }
  const docAtStart = data.text;
  const runId = ++runCounter;
  const queryId = crypto.randomUUID();
  collectors.get(tabId)?.dispose();
  const collector = new ResultCollector(sessionId, () => schedulePublish());
  collectors.set(tabId, collector);
  patchTabData(tabId, { run: { ...snapshotOf(runId, queryId, input, 'running'), docAtStart } });

  let timer: ReturnType<typeof setTimeout> | null = null;
  const publish = () => {
    timer = null;
    updateRun(tabId, runId, {
      results: [...collector.results],
      messages: [...collector.runMessages],
    });
  };
  function schedulePublish(): void {
    timer ??= setTimeout(publish, PUBLISH_MS);
  }
  const finish = (patch: Partial<RunSnapshot>) => {
    if (timer) clearTimeout(timer);
    timer = null;
    updateRun(tabId, runId, {
      results: [...collector.results],
      messages: [...collector.runMessages],
      elapsedMs: Date.now() - (getTabData(tabId)?.run?.startedAt ?? Date.now()),
      ...patch,
    });
    return getTabData(tabId)?.run ?? null;
  };

  let declined = false;
  try {
    const leaseId = await ensureLease(tabId, sessionId);
    const settings = await readSettings();
    const result = await guardedCall(
      (confirm) =>
        ipc.leaseExecute(
          leaseId,
          {
            text: input.text,
            queryId,
            maxRows: settings.editorMaxRows,
            timeoutMs: settings.queryTimeoutMs > 0 ? settings.queryTimeoutMs : null,
            params: input.params ?? [],
            confirm: confirm ?? null,
            recordHistory: settings.historyEnabled,
          },
          (e) => collector.handle(e),
        ),
      (request) =>
        askConfirmation(request).catch((e: unknown) => {
          declined = true;
          throw e;
        }),
    );
    if (result.state) patchTabData(tabId, { leaseState: result.state });
    const pending = result.state?.pendingResult === queryId;
    const model = lastModel(collector);
    if (pending && model) model.onLoadMore = () => void fetchMore(tabId);
    return finish({ status: 'ok', canFetchMore: pending });
  } catch (e) {
    const err = toIpcError(e);
    if (declined) return finish({ status: 'cancelled' });
    if (err.kind === 'cancelled') {
      collector.fail(err);
      await refreshState(tabId);
      return finish({ status: 'cancelled', error: null });
    }
    collector.fail(err);
    const mark = locateError(docAtStart, input.baseChar, err.position, err.message);
    await refreshState(tabId);
    return finish({ status: 'error', error: err, errorMark: mark });
  }
}

/** Cancel the running statement with the protocol's cancel mechanism (QUERY-06). */
export async function cancelRun(tabId: string): Promise<void> {
  const data = getTabData(tabId);
  if (!data?.leaseId || !(data.run?.status === 'running' || data.run?.fetchingMore)) return;
  try {
    await ipc.leaseCancel(data.leaseId, data.run.queryId);
  } catch (e) {
    toast(e);
  }
}

/** Continue the truncated last result set (QUERY-12). */
export async function fetchMore(tabId: string): Promise<void> {
  const data = getTabData(tabId);
  const collector = collectors.get(tabId);
  const run = data?.run;
  if (!data?.leaseId || !run || !collector || !run.canFetchMore || run.fetchingMore) return;
  const last = collector.lastResult;
  if (!last?.model) return;
  updateRun(tabId, run.id, { fetchingMore: true });
  const settings = await readSettings();
  collector.resume(last);
  last.model.markLoading();
  try {
    const result = await ipc.leaseFetchMore(
      data.leaseId,
      run.queryId,
      settings.editorMaxRows,
      null,
      (e) => collector.handle(e),
    );
    if (result.state) patchTabData(tabId, { leaseState: result.state });
    updateRun(tabId, run.id, {
      results: [...collector.results],
      canFetchMore: result.state?.pendingResult === run.queryId,
      fetchingMore: false,
    });
  } catch (e) {
    last.model.finish(true);
    updateRun(tabId, run.id, { fetchingMore: false });
    if (toIpcError(e).kind !== 'cancelled') toast(e);
    await refreshState(tabId);
  }
}

/** The pending result is gone (commit, rollback, next statement): stop offering "more". */
export function dropPending(tabId: string): void {
  const run = getTabData(tabId)?.run;
  if (!run?.canFetchMore) return;
  const collector = collectors.get(tabId);
  if (collector) {
    const last = collector.lastResult;
    last?.model?.finish(false);
  }
  updateRun(tabId, run.id, { canFetchMore: false });
}

// -- transactions -------------------------------------------------------------------------------

/**
 * Run a lease command. `begin` may open the lease; commit, rollback and auto-commit use the
 * lease the transaction lives on and never open a new one (a new connection has no transaction,
 * so "committing" there would report a false success).
 */
async function leaseCall(
  tabId: string,
  sessionId: Uuid,
  call: (leaseId: Uuid) => Promise<LeaseState>,
  mayOpen: boolean,
): Promise<LeaseState> {
  let leaseId: Uuid;
  if (mayOpen) {
    leaseId = await ensureLease(tabId, sessionId);
  } else {
    const data = getTabData(tabId);
    if (!data?.leaseId || data.leaseState?.broken) {
      throw {
        kind: 'connection',
        message: i18n.t('editor:tx.lostError'),
      } satisfies IpcError;
    }
    leaseId = data.leaseId;
  }
  try {
    const state = await call(leaseId);
    patchTabData(tabId, { leaseState: state });
    return state;
  } catch (e) {
    await refreshState(tabId);
    throw toIpcError(e);
  }
}

export async function beginTransaction(
  tabId: string,
  sessionId: Uuid,
  options: TxOptions = {},
): Promise<void> {
  try {
    await leaseCall(tabId, sessionId, (id) => ipc.leaseBegin(id, options), true);
    dropPending(tabId);
  } catch (e) {
    toast(e);
  }
}

/** Commit (or roll back) the open transaction; `true` when it worked. */
export async function endTransaction(
  tabId: string,
  sessionId: Uuid,
  how: 'commit' | 'rollback',
): Promise<boolean> {
  try {
    await leaseCall(
      tabId,
      sessionId,
      (id) => (how === 'commit' ? ipc.leaseCommit(id) : ipc.leaseRollback(id)),
      false,
    );
    dropPending(tabId);
    return true;
  } catch (e) {
    toast(e);
    return false;
  }
}

/**
 * Switch auto-commit. With a transaction open the backend refuses (`busy`): the caller asks the
 * user to commit or roll back first and returns here. Resolves to `'needsDecision'` in that case.
 */
export async function setAutoCommit(
  tabId: string,
  sessionId: Uuid | null,
  on: boolean,
): Promise<'ok' | 'needsDecision' | 'failed'> {
  const data = getTabData(tabId);
  if (!data) return 'failed';
  if (!data.leaseId || sessionId === null || data.leaseState?.broken) {
    patchTabData(tabId, { autoCommit: on });
    return 'ok';
  }
  if (on && hasOpenTransaction(data)) return 'needsDecision';
  try {
    await leaseCall(tabId, sessionId, (id) => ipc.leaseSetAutoCommit(id, on), false);
    patchTabData(tabId, { autoCommit: on });
    dropPending(tabId);
    return 'ok';
  } catch (e) {
    if (toIpcError(e).kind === 'busy') return 'needsDecision';
    toast(e);
    return 'failed';
  }
}

// -- tab lifecycle ------------------------------------------------------------------------------

type CloseAnswer = 'commit' | 'rollback' | 'cancel';

/** Asks what to do with an open transaction when its tab closes; set by the dialog host. */
let askClose: (tabId: string) => Promise<CloseAnswer> = () => Promise.resolve('cancel');

export function setCloseTransactionAsker(ask: (tabId: string) => Promise<CloseAnswer>): void {
  askClose = ask;
}

/**
 * Guard of an editor tab (QUERY-11): with a transaction open the user chooses between commit,
 * rollback and keeping the tab. Resolves to whether the tab may close.
 */
export async function confirmClose(tabStateId: string): Promise<boolean> {
  if (
    hasResultEdits(tabStateId) &&
    !confirmDiscardResultEdits(i18n.t('editor:results.edit.closeConfirm'))
  ) {
    return false;
  }
  const data = getTabData(tabStateId);
  if (!hasOpenTransaction(data)) {
    dropResultEdits(tabStateId);
    return true;
  }
  const answer = await askClose(tabStateId);
  if (answer === 'cancel') return false; // the edits stay too
  dropResultEdits(tabStateId);
  const sessionId = data?.leaseSessionId;
  if (answer === 'commit' && sessionId) {
    return endTransaction(tabStateId, sessionId, 'commit');
  }
  return true; // closing the lease rolls the transaction back
}

const guards = new Map<string, () => void>();

/**
 * The close guard exists only while a transaction may be open, so tabs without one still
 * close synchronously (the tab bar and `requestClose` treat any guard as asynchronous).
 */
function syncGuard(tabStateId: string, data: EditorTabData | undefined): void {
  const needed = hasOpenTransaction(data) || hasResultEdits(tabStateId);
  const registered = guards.get(tabStateId);
  if (needed && !registered) {
    guards.set(
      tabStateId,
      registerTabGuard(editorTabId(tabStateId), () => confirmClose(tabStateId)),
    );
  } else if (!needed && registered) {
    registered();
    guards.delete(tabStateId);
  }
}

/** Start managing an editor tab (close guard follows its transaction). Safe to call twice. */
export function registerEditorTab(tabStateId: string): void {
  syncGuard(tabStateId, getTabData(tabStateId));
}

useEditorStore.subscribe((state, previous) => {
  for (const [id, data] of Object.entries(state.tabs)) {
    if (data !== previous.tabs[id]) syncGuard(id, data);
  }
});

// Unsaved edits in a result grid also keep the close guard in place.
onResultEditsChanged((tabStateId) => syncGuard(tabStateId, getTabData(tabStateId)));

onEditorTabRemoved((tabStateId) => {
  dropResultEdits(tabStateId);
  guards.get(tabStateId)?.();
  guards.delete(tabStateId);
  void closeLease(tabStateId);
  collectors.get(tabStateId)?.dispose();
  collectors.delete(tabStateId);
});

// A closed session takes its leases with it (`SessionManager.close`): forget them here too.
useSessionsStore.subscribe((state) => {
  const live = new Set(Object.values(state.sessionsByConnection).map((s) => s.sessionId));
  for (const tab of Object.values(useEditorStore.getState().tabs)) {
    if (tab.leaseId && tab.leaseSessionId && !live.has(tab.leaseSessionId)) {
      dropLease(tab.id, 'sessionClosed');
    }
  }
});

/** Mark lexer problems of `text` in the tab (QUERY-13). */
export async function analyzeTab(tabId: string, sessionId: Uuid, text: string): Promise<void> {
  try {
    const analysis = await ipc.scriptAnalyze(sessionId, text);
    if (getTabData(tabId)?.text === text) {
      patchTabData(tabId, {
        problems: problemMarks(text, analysis.problems),
        statements: analysis.statements,
        analyzedText: text,
      });
    }
  } catch {
    // analysis is advisory: running reports real errors
  }
}

/** Test helper: forget in-flight state. */
export function resetEditorRunner(): void {
  for (const c of collectors.values()) c.dispose();
  collectors.clear();
  opening.clear();
  for (const off of guards.values()) off();
  guards.clear();
  askClose = () => Promise.resolve('cancel');
}
