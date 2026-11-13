import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import type { ConfirmationRequest, PlanNode, PlanResult } from '@/lib/types';
import { planTabId, useTabsStore, WELCOME_TAB } from '@/stores/tabs';
import { polyfillLayoutApis } from '../data/testUtils';
import { initialCollapsed } from './graphModel';
import { layoutPlan } from './elkLayout';
import { ConfirmDestructiveHost } from '@/features/safety/ConfirmDestructiveHost';
import { PlanTab } from './PlanTab';
import { cancelPlan, runPlan, usePlanStore } from './planStore';
import { isWriteStatement, statementWrites } from './planModel';
import { rawOf, SAMPLES, seedPlan, TREES } from './planTestData';

vi.mock('./PlanGraph', () => ({ PlanGraph: () => <div data-testid="plan-graph" /> }));

beforeAll(() => polyfillLayoutApis());
beforeEach(async () => {
  usePlanStore.setState({ entries: {} });
  useTabsStore.setState({ tabs: [WELCOME_TAB], activeId: WELCOME_TAB.id });
  await i18n.changeLanguage('en');
});
afterEach(() => vi.restoreAllMocks());

const tree = TREES[SAMPLES.pg.file]!;
const answer = (): PlanResult => ({
  raw: rawOf(SAMPLES.pg.format, SAMPLES.pg.text),
  tree: structuredClone(tree),
  parseError: null,
});
const confirmation: ConfirmationRequest = {
  token: 'tok',
  confirmText: 'shop',
  statements: [
    {
      index: 0,
      range: { start: 0, end: 10 },
      text: 'UPDATE t SET a = 1',
      reason: 'updateWithoutWhere',
    },
  ],
  expiresInMs: 60_000,
};

describe('cancelling', () => {
  it('cancel stops the running statement through its query id', async () => {
    const cancel = vi.spyOn(ipc, 'queryCancel').mockResolvedValue();
    const id = seedPlan({ status: 'loading' });
    cancelPlan(id);
    expect(cancel).toHaveBeenCalledWith('s1', 'q1');
    // Nothing runs: nothing to cancel.
    cancel.mockClear();
    cancelPlan(seedPlan({ status: 'ready' }));
    expect(cancel).not.toHaveBeenCalled();
  });

  it('a cancelled run keeps the previous plan on screen', async () => {
    vi.spyOn(ipc, 'planExplain').mockRejectedValue({
      kind: 'cancelled',
      message: 'operation cancelled',
    });
    const id = seedPlan();
    await runPlan(id);
    const e = usePlanStore.getState().entries[id]!;
    expect(e.status).toBe('ready');
    expect(e.tree).not.toBeNull();
    // Without a previous plan it is an error screen.
    const fresh = seedPlan({ raw: null, tree: null });
    await runPlan(fresh);
    expect(usePlanStore.getState().entries[fresh]!.status).toBe('error');
  });

  it('closing the tab of a running statement cancels it', () => {
    const cancel = vi.spyOn(ipc, 'queryCancel').mockResolvedValue();
    const id = seedPlan({ status: 'loading' });
    useTabsStore.getState().openTab({
      id: planTabId(id),
      kind: 'plan',
      title: 'p',
      closable: true,
      payload: { sessionId: 's1', connectionId: 'c1', planId: id },
    });
    expect(cancel).not.toHaveBeenCalled();
    useTabsStore.getState().closeTab(planTabId(id));
    expect(cancel).toHaveBeenCalledWith('s1', 'q1');
    expect(usePlanStore.getState().entries[id]).toBeUndefined();
  });

  it('shows a Cancel button while loading that cancels', async () => {
    const cancel = vi.spyOn(ipc, 'queryCancel').mockResolvedValue();
    const id = seedPlan({ status: 'loading', raw: null, tree: null });
    render(
      <Providers>
        <PlanTab planId={id} sessionId="s1" connectionId="c1" />
      </Providers>,
    );
    await userEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(cancel).toHaveBeenCalledWith('s1', 'q1');
  });
});

describe('production confirmation', () => {
  it('stores the confirmation request and asks the user, then repeats with the grant', async () => {
    const explain = vi
      .spyOn(ipc, 'planExplain')
      .mockRejectedValueOnce({ kind: 'confirmationRequired', message: 'confirm', confirmation })
      .mockResolvedValueOnce(answer());
    const id = seedPlan({
      raw: null,
      tree: null,
      statement: 'UPDATE t SET a = 1',
      request: { queryId: 'q1', text: 'UPDATE t SET a = 1', analyze: false, options: {} },
    });
    const user = userEvent.setup();
    render(
      <Providers>
        <PlanTab planId={id} sessionId="s1" connectionId="c1" />
        <ConfirmDestructiveHost />
      </Providers>,
    );
    vi.spyOn(ipc, 'scriptAnalyze').mockResolvedValue({
      dialect: { name: 'postgres' },
      statements: [],
      params: [],
      problems: [],
    });
    void runPlan(id, { analyze: true });
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText('UPDATE t SET a = 1')).toBeInTheDocument();
    const run = within(dialog).getByRole('button', { name: 'Run anyway' });
    expect(run).toBeDisabled();
    await user.type(within(dialog).getByRole('textbox'), 'sho');
    expect(run).toBeDisabled();
    await user.type(within(dialog).getByRole('textbox'), 'p');
    await user.click(run);
    await waitFor(() => expect(explain).toHaveBeenCalledTimes(2));
    expect(explain.mock.calls[1]![4]).toEqual({ token: 'tok', typed: 'shop' });
    expect(explain.mock.calls[1]![2].analyze).toBe(true);
    await waitFor(() => expect(usePlanStore.getState().entries[id]!.status).toBe('ready'));
  });
});

describe('production confirmation declined', () => {
  it('keeps the previous plan and does not repeat the call', async () => {
    const explain = vi
      .spyOn(ipc, 'planExplain')
      .mockRejectedValue({ kind: 'confirmationRequired', message: 'confirm', confirmation });
    const id = seedPlan();
    const user = userEvent.setup();
    render(
      <Providers>
        <ConfirmDestructiveHost />
      </Providers>,
    );
    const run = runPlan(id, { analyze: true });
    const dialog = await screen.findByRole('dialog');
    await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await run;
    expect(explain).toHaveBeenCalledTimes(1);
    const e = usePlanStore.getState().entries[id]!;
    expect(e.status).toBe('ready');
    expect(e.tree).not.toBeNull();
  });
});

describe('write detection', () => {
  it.each([
    ['WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x', true],
    ['with x as (update t set a=1 returning a) select * from x', true],
    ['SELECT * FROM t FOR UPDATE', true],
    ["SELECT replace(a, 'x', 'y'), insert('abc', 1, 1, 'z') FROM t", false],
    ['SELECT pg_advisory_lock(1)', true],
    ['SELECT GET_LOCK("a", 1)', true],
    ["SELECT nextval('s')", true],
    ["SELECT dblink('c', 'select 1')", true],
    ['SELECT * INTO t2 FROM t', true],
    ["SELECT 'nextval(1)' FROM t", false],
    ['SELECT a FROM t WHERE b IN (SELECT c FROM u)', false],
  ])('%s -> %s', (sql, expected) => {
    expect(isWriteStatement(sql)).toBe(expected);
  });

  it('prefers the backend analysis and falls back to the local guess', async () => {
    const analyze = vi.spyOn(ipc, 'scriptAnalyze');
    analyze.mockResolvedValueOnce({
      dialect: { name: 'postgres' },
      statements: [
        {
          index: 0,
          range: { start: 0, end: 5 },
          class: { kind: 'write' },
          verb: 'X',
          destructive: null,
          implicitCommit: false,
          targets: [],
        },
      ],
      params: [],
      problems: [],
    });
    expect(await statementWrites('s', 'select 1')).toBe(true);
    analyze.mockResolvedValueOnce({
      dialect: { name: 'postgres' },
      statements: [
        {
          index: 0,
          range: { start: 0, end: 5 },
          class: { kind: 'read' },
          verb: 'SELECT',
          destructive: null,
          implicitCommit: false,
          targets: [],
        },
      ],
      params: [],
      problems: [],
    });
    expect(await statementWrites('s', 'select 1')).toBe(false);
    analyze.mockRejectedValueOnce({ kind: 'notFound', message: 'x' });
    expect(await statementWrites('s', 'delete from t')).toBe(true);
  });
});

describe('big plans', () => {
  const leaf = (id: number): PlanNode => ({ ...structuredClone(tree.root), id, children: [] });

  it('collapses the level that does not fit', () => {
    const root = { ...leaf(0), children: Array.from({ length: 400 }, (_, i) => leaf(i + 1)) };
    const wide = { ...tree, root, nodeCount: 401 };
    expect([...initialCollapsed(wide)]).toEqual([0]);
    expect(initialCollapsed(tree).size).toBe(0);
    // Two levels fit, the third does not.
    const mid = {
      ...leaf(0),
      children: Array.from({ length: 10 }, (_, i) => ({
        ...leaf(i + 1),
        children: Array.from({ length: 40 }, (_, j) => leaf(100 + i * 40 + j)),
      })),
    };
    const deep = { ...tree, root: mid, nodeCount: 411 };
    expect(initialCollapsed(deep, 300).size).toBe(10);
    expect(initialCollapsed(deep, 500).size).toBe(0);
  });
});

describe('layout', () => {
  it('lays out every visible node (main thread engine in tests)', async () => {
    const l = await layoutPlan(tree.root, new Set());
    expect(l.positions.size).toBe(tree.nodeCount);
    expect(l.width).toBeGreaterThan(0);
    // Root on the left, leaves further right.
    const xs = [...l.positions.values()].map((p) => p.x);
    expect(l.positions.get(0)!.x).toBe(Math.min(...xs));
    const small = await layoutPlan(tree.root, new Set([2]));
    expect(small.positions.size).toBe(3);
  });
});
