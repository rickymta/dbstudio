import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ipc } from '@/lib/ipc';
import type { PlanResult } from '@/lib/types';
import { planTabId, useTabsStore, WELCOME_TAB } from '@/stores/tabs';
import { openPlan, openRawPlan } from './open';
import { loadRawPlan, runPlan, usePlanStore } from './planStore';
import { rawOf, SAMPLES, seedPlan, TREES } from './planTestData';

const result = (): PlanResult => ({
  raw: rawOf(SAMPLES.pg.format, SAMPLES.pg.text),
  tree: structuredClone(TREES[SAMPLES.pg.file]!),
  parseError: null,
});

beforeEach(() => {
  usePlanStore.setState({ entries: {} });
  useTabsStore.setState({ tabs: [WELCOME_TAB], activeId: WELCOME_TAB.id });
});
afterEach(() => vi.restoreAllMocks());

describe('runPlan', () => {
  it('goes loading then ready and clears the selection', async () => {
    const spy = vi.spyOn(ipc, 'planExplain').mockResolvedValue(result());
    const id = seedPlan({ raw: null, tree: null, status: 'ready', selectedId: 4 });
    const run = runPlan(id, { analyze: false });
    expect(usePlanStore.getState().entries[id]!.status).toBe('loading');
    await run;
    const e = usePlanStore.getState().entries[id]!;
    expect(e.status).toBe('ready');
    expect(e.tree?.nodeCount).toBe(6);
    expect(e.selectedId).toBeNull();
    // The override is remembered, each run gets a fresh query id.
    expect(e.request?.analyze).toBe(false);
    expect(spy.mock.calls[0]![2].queryId).not.toBe('q1');
    expect(spy.mock.calls[0]![0]).toBe('s1');
  });

  it('stores the error of a failed run', async () => {
    vi.spyOn(ipc, 'planExplain').mockRejectedValue({
      kind: 'unsupported',
      message: 'x',
      capability: 'explain_analyze_writes',
    });
    const id = seedPlan();
    await runPlan(id);
    const e = usePlanStore.getState().entries[id]!;
    expect(e.status).toBe('error');
    expect(e.error?.capability).toBe('explain_analyze_writes');
  });

  it('ignores the answer of an older run', async () => {
    let first!: (r: PlanResult) => void;
    vi.spyOn(ipc, 'planExplain')
      .mockImplementationOnce(() => new Promise((res) => (first = res)))
      .mockResolvedValueOnce({ ...result(), parseError: 'second' });
    const id = seedPlan({ raw: null, tree: null });
    const a = runPlan(id);
    await runPlan(id);
    first({ ...result(), parseError: 'first' });
    await a;
    expect(usePlanStore.getState().entries[id]!.parseError).toBe('second');
  });

  it('does nothing for plans without a request', async () => {
    const spy = vi.spyOn(ipc, 'planExplain');
    const id = seedPlan({ request: null });
    await runPlan(id);
    expect(spy).not.toHaveBeenCalled();
  });
});

describe('loadRawPlan', () => {
  it('parses a raw plan, and keeps the raw text when parsing fails', async () => {
    const raw = rawOf(SAMPLES.pg.format, SAMPLES.pg.text);
    vi.spyOn(ipc, 'planParse').mockResolvedValueOnce(structuredClone(TREES[SAMPLES.pg.file]!));
    const id = seedPlan({ request: null, raw: null, tree: null });
    await loadRawPlan(id, raw);
    expect(usePlanStore.getState().entries[id]!.tree).not.toBeNull();

    vi.spyOn(ipc, 'planParse').mockRejectedValueOnce({ kind: 'other', message: 'invalid plan: x' });
    await loadRawPlan(id, raw);
    const e = usePlanStore.getState().entries[id]!;
    expect(e.tree).toBeNull();
    expect(e.raw).toEqual(raw);
    expect(e.parseError).toBe('invalid plan: x');
    expect(e.status).toBe('ready');
  });
});

describe('opening plans', () => {
  it('opens a tab at once and fills it when the answer arrives', async () => {
    vi.spyOn(ipc, 'planExplain').mockResolvedValue(result());
    const planId = openPlan({
      session: { sessionId: 's1', connectionId: 'c1' },
      text: 'select 1',
      analyze: false,
      leaseId: 'lease-1',
    });
    const tab = useTabsStore.getState().tabs.find((t) => t.id === planTabId(planId))!;
    expect(tab.kind).toBe('plan');
    expect(usePlanStore.getState().entries[planId]!.status).toBe('loading');
    expect(usePlanStore.getState().entries[planId]!.leaseId).toBe('lease-1');
    await vi.waitFor(() => expect(usePlanStore.getState().entries[planId]!.status).toBe('ready'));
    expect(ipc.planExplain).toHaveBeenCalledWith(
      's1',
      'lease-1',
      expect.objectContaining({ text: 'select 1', analyze: false }),
      null,
      null,
    );
  });

  it('opens a plan from raw text through planParse', async () => {
    vi.spyOn(ipc, 'planParse').mockResolvedValue(structuredClone(TREES[SAMPLES.pg.file]!));
    const planId = openRawPlan(
      { sessionId: 's1', connectionId: 'c1' },
      rawOf(SAMPLES.pg.format, SAMPLES.pg.text, 'select 2'),
    );
    expect(usePlanStore.getState().entries[planId]!.statement).toBe('select 2');
    await vi.waitFor(() => expect(usePlanStore.getState().entries[planId]!.tree).not.toBeNull());
  });

  it('forgets the plan when its tab is closed', async () => {
    vi.spyOn(ipc, 'planExplain').mockResolvedValue(result());
    const planId = openPlan({ session: { sessionId: 's1', connectionId: 'c1' }, text: 'select 1' });
    await vi.waitFor(() => expect(usePlanStore.getState().entries[planId]!.status).toBe('ready'));
    useTabsStore.getState().closeTab(planTabId(planId));
    expect(usePlanStore.getState().entries[planId]).toBeUndefined();
  });
});

describe('selection', () => {
  it('select sets and clears the shared node', () => {
    const id = seedPlan();
    usePlanStore.getState().select(id, 3);
    expect(usePlanStore.getState().entries[id]!.selectedId).toBe(3);
    usePlanStore.getState().select(id, null);
    expect(usePlanStore.getState().entries[id]!.selectedId).toBeNull();
    // Unknown plans are ignored.
    usePlanStore.getState().select('nope', 1);
  });
});
