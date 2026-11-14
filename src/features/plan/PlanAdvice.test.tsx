import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import { useEditorStore, newTabData } from '@/features/editor/editorStore';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { DEFAULT_SETTINGS } from '@/lib/types';
import type {
  AdviceReport,
  ConnectionProfile,
  PlanAdvice,
  PlanNode,
  PlanTree,
  SessionInfo,
} from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { editorTabId, useTabsStore, WELCOME_TAB, type EditorTabPayload } from '@/stores/tabs';
import { polyfillLayoutApis } from '../data/testUtils';
import { makeAdvice } from './adviceTestData';
import { PlanCompare } from './PlanCompare';
import { PlanTab } from './PlanTab';
import { useAdviceStore } from './PlanAdviceData';
import { usePlanStore } from './planStore';
import { SAMPLES, seedPlan, TREES } from './planTestData';

vi.mock('./elkLayout', () => ({
  NODE_WIDTH: 240,
  NODE_HEIGHT: 76,
  layoutPlan: async (root: PlanNode, collapsed: ReadonlySet<number>) => {
    const positions = new Map<number, { x: number; y: number }>();
    let row = 0;
    const walk = (n: PlanNode, depth: number) => {
      positions.set(n.id, { x: depth * 300, y: row++ * 100 });
      if (!collapsed.has(n.id)) n.children.forEach((c) => walk(c, depth + 1));
    };
    walk(root, 0);
    return { positions, width: 1200, height: row * 100 };
  },
}));

beforeAll(() => {
  polyfillLayoutApis();
  if (!('DOMMatrixReadOnly' in globalThis)) {
    class Matrix {
      m22 = 1;
      constructor(public transform?: string) {}
    }
    (globalThis as unknown as { DOMMatrixReadOnly: unknown }).DOMMatrixReadOnly = Matrix;
  }
});

const INDEX_SQL = 'CREATE INDEX CONCURRENTLY ix_orders_status ON orders (status);';
const createIndex = {
  kind: 'createIndex' as const,
  labelKey: 'createIndex',
  sql: INDEX_SQL,
  template: false,
  scope: 'object' as const,
  lock: 'shortLock' as const,
};
const review = {
  kind: 'check' as const,
  labelKey: 'reviewFilter',
  template: false,
  scope: 'query' as const,
};

const ADVICE: PlanAdvice[] = [
  makeAdvice({
    id: 'indexForFilter:aaaa',
    rule: 'indexForFilter',
    nodeIds: [3],
    impact: { basis: 'time', share: 0.78, score: 0.78 },
    params: {
      table: 'orders',
      columns: 'status',
      examined: '90000',
      kept: '10',
      selectivity: '0.1%',
    },
    evidence: [
      { key: 'rowsRead', nodeId: 3, value: 90000, unit: 'rows', actual: true },
      { key: 'selectivity', nodeId: 3, value: 40, unit: 'pct', actual: false },
    ],
    actions: [createIndex, review],
    caveats: [{ key: 'writeOverhead', verify: false }],
    table: {
      name: 'orders',
      schema: 'public',
      path: {
        segments: [
          { kind: 'schema', name: 'public' },
          { kind: 'table', name: 'orders' },
        ],
      },
    },
  }),
  makeAdvice({
    id: 'staleStatistics:bbbb',
    rule: 'staleStatistics',
    severity: 'info',
    confidence: 'medium',
    nodeIds: [5],
    impact: { basis: 'time', share: 0.2, score: 0.14 },
    params: { table: 'customers', estimated: '1', actual: '9', factor: '9' },
    fingerprint: 'stats|customers',
  }),
  makeAdvice({
    id: 'selectStar:cccc',
    rule: 'selectStar',
    severity: 'info',
    confidence: 'low',
    nodeIds: [],
    impact: { basis: 'none', score: 0.01 },
    params: { sign: 'spill' },
    caveats: [{ key: 'estimatedOnly', verify: true }],
    fingerprint: 'star',
  }),
];

const report = (
  advice: PlanAdvice[],
  over: Partial<AdviceReport['metadata']> = {},
): AdviceReport => ({
  advice,
  metadata: { state: 'checked', checked: [], failed: [], skipped: 0, elapsedMs: 3, ...over },
});

function adviceTree(advice: PlanAdvice[] = ADVICE): PlanTree {
  const tree = structuredClone(TREES[SAMPLES.pg.file]!);
  return { ...tree, advice: structuredClone(advice) };
}

function profile(over: Partial<ConnectionProfile> = {}): ConnectionProfile {
  return {
    id: 'c1',
    name: 'Shop',
    kind: 'postgres',
    host: 'h',
    port: 1,
    tls: { mode: 'disable' },
    readOnly: false,
    options: {},
    ...over,
  } as ConnectionProfile;
}

const fakeSession = {
  sessionId: 's1',
  connectionId: 'c1',
  dialect: { name: 'postgres' },
  descriptor: { capabilities: {} },
} as unknown as SessionInfo;

let advise: ReturnType<typeof vi.spyOn>;

beforeEach(async () => {
  usePlanStore.setState({ entries: {} });
  useAdviceStore.setState({ slots: {} });
  useTabsStore.setState({ tabs: [WELCOME_TAB], activeId: WELCOME_TAB.id });
  useSessionsStore.setState({ sessionsByConnection: {} });
  try {
    localStorage.clear();
  } catch {
    /* ignore */
  }
  await i18n.changeLanguage('en');
  vi.spyOn(ipc, 'settingsGet').mockResolvedValue({ ...DEFAULT_SETTINGS });
  vi.spyOn(ipc, 'connectionsList').mockResolvedValue({ groups: [], profiles: [profile()] });
  advise = vi.spyOn(ipc, 'planAdvise').mockResolvedValue(report(ADVICE));
});
afterEach(() => vi.restoreAllMocks());

function mount(planId: string) {
  const user = userEvent.setup();
  render(
    <Providers>
      <PlanTab planId={planId} sessionId="s1" connectionId="c1" />
    </Providers>,
  );
  return user;
}

/** `answer`: what `planAdvise` returns (default: the same advice, matched against the catalog). */
const seed = (
  advice = ADVICE,
  patch = {},
  answer: AdviceReport | (() => Promise<AdviceReport>) = report(advice),
) => {
  if (typeof answer === 'function') advise.mockImplementation(answer);
  else advise.mockResolvedValue(answer);
  return seedPlan({ tree: adviceTree(advice), ...patch });
};
const selected = (id: string) => usePlanStore.getState().entries[id]!.selectedId;
const cardIds = () =>
  screen.queryAllByTestId(/^advice-card-/).map((el) => el.getAttribute('data-testid')!.slice(12));

describe('advice cards', () => {
  it('lists the advice ranked by impact with the confidence labels', async () => {
    mount(seed([ADVICE[2]!, ADVICE[1]!, ADVICE[0]!]));
    const panel = await screen.findByTestId('advice-panel');
    await waitFor(() =>
      expect(cardIds()).toEqual(['indexForFilter:aaaa', 'staleStatistics:bbbb', 'selectStar:cccc']),
    );
    const labels = within(panel)
      .getAllByTestId('advice-confidence')
      .map((e) => [e.getAttribute('data-confidence'), e.textContent]);
    expect(labels).toEqual([
      ['high', 'High confidence'],
      ['medium', 'Medium'],
      ['verify', 'Needs verification'],
    ]);
    expect(within(panel).getByText('Add an index for the filter on orders')).toBeInTheDocument();
    expect(
      within(screen.getByTestId('advice-card-indexForFilter:aaaa')).getByTestId('advice-share'),
    ).toHaveTextContent('~78%');
  });

  it('opens a card to show why, the evidence, actions and caveats', async () => {
    const user = mount(seed());
    const card = await screen.findByTestId('advice-card-indexForFilter:aaaa');
    expect(within(card).queryByTestId('advice-evidence')).toBeNull();
    await user.click(within(card).getAllByRole('button')[0]!);
    const evidence = within(card).getByTestId('advice-evidence');
    expect(within(evidence).getByText('Rows read')).toBeInTheDocument();
    expect(within(evidence).getByText('90,000')).toBeInTheDocument();
    // An estimate is marked as such, a measured figure is not.
    expect(within(evidence).getByText('(estimate)')).toBeInTheDocument();
    expect(within(card).getByText(/reads 90000 rows but keeps only 10/)).toBeInTheDocument();
    expect(within(card).getByTestId('advice-sql')).toHaveTextContent(INDEX_SQL);
    expect(within(card).getByText('Object')).toBeInTheDocument();
    expect(within(card).getByText('Short lock')).toBeInTheDocument();
    expect(within(card).getByTestId('advice-caveats')).toHaveTextContent(/slows down inserts/);
    // An action without a statement is a description only.
    expect(within(card).getAllByTestId('advice-action')).toHaveLength(2);
    expect(within(card).getAllByRole('button', { name: /Copy/ })).toHaveLength(1);
  });

  it('shows the summary strip and opens the biggest advice from "View"', async () => {
    const user = mount(seed());
    const strip = await screen.findByTestId('advice-summary');
    expect(within(strip).getByTestId('advice-summary-text')).toHaveTextContent(
      '3 optimisation points · biggest: Add an index for the filter on orders — the node takes ~78% of the time',
    );
    await user.click(within(strip).getByRole('button', { name: 'View' }));
    expect(
      selected(usePlanStore.getState().entries && Object.keys(usePlanStore.getState().entries)[0]!),
    ).toBe(3);
    expect(
      within(screen.getByTestId('advice-card-indexForFilter:aaaa')).getByTestId('advice-evidence'),
    ).toBeInTheDocument();
  });

  it('says so when there is nothing to optimise, and the note can be hidden', async () => {
    const user = mount(seed([]));
    expect(await screen.findByTestId('advice-summary-none')).toHaveTextContent(
      'No clear optimisation point found',
    );
    await user.click(screen.getByRole('button', { name: 'Hide' }));
    expect(screen.queryByTestId('advice-summary-none')).toBeNull();
  });
});

describe('advice and nodes', () => {
  it('selecting a card selects its first node, also in the table, and opens hidden branches', async () => {
    const id = seed();
    const user = mount(id);
    await user.click(screen.getByRole('tab', { name: 'Table' }));
    await user.click(await screen.findByRole('button', { name: 'Collapse all' }));
    expect(screen.queryByTestId('plan-row-3')).toBeNull();
    await user.click(
      within(await screen.findByTestId('advice-card-indexForFilter:aaaa')).getAllByRole(
        'button',
      )[0]!,
    );
    expect(selected(id)).toBe(3);
    const row = await screen.findByTestId('plan-row-3');
    expect(row).toHaveAttribute('aria-selected', 'true');
    // Advice without a node changes nothing.
    await user.click(
      within(screen.getByTestId('advice-card-selectStar:cccc')).getAllByRole('button')[0]!,
    );
    expect(selected(id)).toBe(3);
  });

  it('selects the node in the diagram and shows its lightbulb there too', async () => {
    const id = seed();
    const user = mount(id);
    expect(await screen.findByTestId('advice-mark-3')).toBeInTheDocument();
    await user.click(
      within(await screen.findByTestId('advice-card-indexForFilter:aaaa')).getAllByRole(
        'button',
      )[0]!,
    );
    expect(selected(id)).toBe(3);
    expect(await screen.findByTestId('plan-node-3')).toHaveAttribute('aria-pressed', 'true');
  });

  it('marks nodes that carry advice and lists only their advice when clicked', async () => {
    const id = seed();
    const user = mount(id);
    await user.click(screen.getByRole('tab', { name: 'Table' }));
    expect(await screen.findByTestId('advice-mark-3')).toBeInTheDocument();
    expect(screen.getByTestId('advice-mark-5')).toBeInTheDocument();
    expect(screen.queryByTestId('advice-mark-4')).toBeNull();
    await user.click(await screen.findByTestId('plan-row-5'));
    await waitFor(() => expect(cardIds()).toEqual(['staleStatistics:bbbb']));
    expect(screen.getByTestId('advice-filter')).toHaveTextContent('node #5');
    // A node without advice shows everything again.
    await user.click(screen.getByTestId('plan-row-4'));
    await waitFor(() => expect(cardIds()).toHaveLength(3));
    await user.click(screen.getByTestId('plan-row-3'));
    await user.click(
      within(screen.getByTestId('advice-filter')).getByRole('button', { name: 'Show all' }),
    );
    expect(cardIds()).toHaveLength(3);
  });

  it('selecting a node in the diagram also filters', async () => {
    mount(seed());
    fireEvent.click(await screen.findByTestId('plan-node-3'));
    await waitFor(() => expect(cardIds()).toEqual(['indexForFilter:aaaa']));
  });

  it('keeps the warnings in the node tab only when there is no advice', async () => {
    const user = mount(seed());
    await screen.findByTestId('advice-panel');
    await user.click(screen.getByRole('tab', { name: 'Node' }));
    expect(screen.queryByText(/Warnings:/)).toBeNull();
  });
});

describe('actions', () => {
  it('copies the statement', async () => {
    const user = mount(seed());
    const write = vi.spyOn(navigator.clipboard, 'writeText').mockResolvedValue();
    const card = await screen.findByTestId('advice-card-indexForFilter:aaaa');
    await user.click(within(card).getAllByRole('button')[0]!);
    await user.click(within(card).getByRole('button', { name: /Copy/ }));
    expect(write).toHaveBeenCalledWith(INDEX_SQL);
  });

  it('copies the first statement of the focused card with the c key', async () => {
    const user = mount(seed());
    const write = vi.spyOn(navigator.clipboard, 'writeText').mockResolvedValue();
    const card = await screen.findByTestId('advice-card-indexForFilter:aaaa');
    within(card).getAllByRole('button')[0]!.focus();
    await user.keyboard('c');
    expect(write).toHaveBeenCalledWith(INDEX_SQL);
  });

  it('moves between cards with the arrow keys', async () => {
    const user = mount(seed());
    const first = within(await screen.findByTestId('advice-card-indexForFilter:aaaa')).getAllByRole(
      'button',
    )[0]!;
    first.focus();
    await user.keyboard('{ArrowDown}');
    expect(document.activeElement).toBe(
      within(screen.getByTestId('advice-card-staleStatistics:bbbb')).getAllByRole('button')[0],
    );
    await user.keyboard('{ArrowUp}');
    expect(document.activeElement).toBe(first);
  });

  function openEditorTab(text = 'select * from orders') {
    useEditorStore.getState().ensure('ed1', newTabData('ed1', { text, title: 'Query 1' }));
    useTabsStore.getState().openTab({
      id: editorTabId('ed1'),
      kind: 'editor',
      title: 'Query 1',
      closable: true,
      payload: { tabStateId: 'ed1', connectionId: 'c1', sessionId: 's1', mode: 'sql' },
    });
    useTabsStore.getState().activate(WELCOME_TAB.id);
  }

  it('appends to the source editor after a blank line, selects it and does not run it', async () => {
    openEditorTab();
    const run = vi.spyOn(ipc, 'queryExecute');
    const user = mount(seed(ADVICE, { sourceEditorId: 'ed1' }));
    const card = await screen.findByTestId('advice-card-indexForFilter:aaaa');
    await user.click(within(card).getAllByRole('button')[0]!);
    await user.click(within(card).getByRole('button', { name: /Insert into editor/ }));
    const data = useEditorStore.getState().tabs.ed1!;
    expect(data.text).toBe(`select * from orders;\n\n${INDEX_SQL}`);
    expect(data.selection).toEqual({ from: 23, to: 23 + INDEX_SQL.length });
    expect(useTabsStore.getState().activeId).toBe(editorTabId('ed1'));
    expect(run).not.toHaveBeenCalled();
  });

  it('appends to a source editor restored after a reload (no session id in its tab)', async () => {
    openEditorTab();
    useTabsStore.setState((st) => ({
      tabs: st.tabs.map((x) =>
        x.id === editorTabId('ed1') && x.kind === 'editor'
          ? { ...x, payload: { ...x.payload, sessionId: null } }
          : x,
      ),
    }));
    const before = useTabsStore.getState().tabs.length;
    const user = mount(seed(ADVICE, { sourceEditorId: 'ed1' }));
    const card = await screen.findByTestId('advice-card-indexForFilter:aaaa');
    await user.click(within(card).getAllByRole('button')[0]!);
    await user.click(within(card).getByRole('button', { name: /Insert into editor/ }));
    expect(useEditorStore.getState().tabs.ed1!.text).toBe(`select * from orders;\n\n${INDEX_SQL}`);
    expect(useTabsStore.getState().tabs.length).toBe(before);
  });

  it('opens a new editor with a distinct title when the source is on another connection', async () => {
    openEditorTab();
    useTabsStore.setState((st) => ({
      tabs: st.tabs.map((x) =>
        x.id === editorTabId('ed1') && x.kind === 'editor'
          ? { ...x, payload: { ...x.payload, connectionId: 'other' } }
          : x,
      ),
    }));
    useSessionsStore.setState({ sessionsByConnection: { c1: fakeSession } });
    const user = mount(seed(ADVICE, { sourceEditorId: 'ed1' }));
    const card = await screen.findByTestId('advice-card-indexForFilter:aaaa');
    await user.click(within(card).getAllByRole('button')[0]!);
    await user.click(within(card).getByRole('button', { name: /Insert into editor/ }));
    expect(useEditorStore.getState().tabs.ed1!.text).toBe('select * from orders');
    const active = useTabsStore
      .getState()
      .tabs.find((t) => t.id === useTabsStore.getState().activeId)!;
    const created =
      useEditorStore.getState().tabs[(active.payload as EditorTabPayload).tabStateId]!;
    expect(created.title).not.toBe('Query 1');
  });

  it('puts the cursor after a template instead of selecting it', async () => {
    openEditorTab('');
    const template = makeAdvice({
      id: 'tpl:1',
      nodeIds: [3],
      actions: [{ ...createIndex, sql: 'CREATE INDEX ix ON t (/* columns */);', template: true }],
      fingerprint: 'tpl',
    });
    const user = mount(seed([template], { sourceEditorId: 'ed1' }));
    const card = await screen.findByTestId('advice-card-tpl:1');
    await user.click(within(card).getAllByRole('button')[0]!);
    expect(within(card).getByTestId('advice-template')).toHaveTextContent(
      'Template — edit before running',
    );
    await user.click(within(card).getByRole('button', { name: /Insert into editor/ }));
    const data = useEditorStore.getState().tabs.ed1!;
    expect(data.text).toBe('CREATE INDEX ix ON t (/* columns */);');
    expect(data.selection.from).toBe(data.selection.to);
  });

  it('opens a new editor on the connection when the source editor is gone', async () => {
    useSessionsStore.setState({ sessionsByConnection: { c1: fakeSession } });
    const user = mount(seed(ADVICE, { sourceEditorId: 'closed' }));
    const card = await screen.findByTestId('advice-card-indexForFilter:aaaa');
    await user.click(within(card).getAllByRole('button')[0]!);
    await user.click(within(card).getByRole('button', { name: /Insert into editor/ }));
    const active = useTabsStore
      .getState()
      .tabs.find((t) => t.id === useTabsStore.getState().activeId)!;
    expect(active.kind).toBe('editor');
    const stateId = (active.payload as EditorTabPayload).tabStateId;
    const data = useEditorStore.getState().tabs[stateId]!;
    expect(data.text).toBe(INDEX_SQL);
    expect(data.selection).toEqual({ from: 0, to: INDEX_SQL.length });
  });

  it('opens the table of the advice', async () => {
    useSessionsStore.setState({ sessionsByConnection: { c1: fakeSession } });
    const user = mount(seed());
    const card = await screen.findByTestId('advice-card-indexForFilter:aaaa');
    await user.click(within(card).getAllByRole('button')[0]!);
    await user.click(within(card).getByRole('button', { name: 'Open table' }));
    const tab = useTabsStore.getState().tabs.find((t) => t.kind === 'table');
    expect(tab?.title).toBe('orders');
  });

  it('disables "Open table" without an open connection', async () => {
    const user = mount(seed());
    const card = await screen.findByTestId('advice-card-indexForFilter:aaaa');
    await user.click(within(card).getAllByRole('button')[0]!);
    expect(within(card).getByRole('button', { name: 'Open table' })).toBeDisabled();
  });
});

describe('read-only and production connections', () => {
  async function openFirstCard() {
    const user = userEvent.setup();
    const card = await screen.findByTestId('advice-card-indexForFilter:aaaa');
    await user.click(within(card).getAllByRole('button')[0]!);
    return card;
  }

  it('warns before inserting DDL on a read-only connection', async () => {
    vi.spyOn(ipc, 'connectionsList').mockResolvedValue({
      groups: [],
      profiles: [profile({ readOnly: true })],
    });
    mount(seed());
    const card = await openFirstCard();
    expect(await within(card).findByTestId('advice-readonly')).toHaveTextContent(
      'Read-only connection: this statement will be blocked when run',
    );
    expect(within(card).queryByTestId('advice-production')).toBeNull();
  });

  it('marks DDL actions, but not descriptions, on a production connection', async () => {
    vi.spyOn(ipc, 'connectionsList').mockResolvedValue({
      groups: [],
      profiles: [profile({ environment: 'production' })],
    });
    mount(seed());
    const card = await openFirstCard();
    await waitFor(() => expect(within(card).getAllByTestId('advice-production')).toHaveLength(1));
    const actions = within(card).getAllByTestId('advice-action');
    expect(within(actions[0]!).getByTestId('advice-production')).toHaveTextContent('Production');
    expect(within(actions[1]!).queryByTestId('advice-production')).toBeNull();
    expect(within(card).queryByTestId('advice-readonly')).toBeNull();
  });

  it('shows neither on an ordinary connection', async () => {
    mount(seed());
    const card = await openFirstCard();
    expect(within(card).queryByTestId('advice-production')).toBeNull();
    expect(within(card).queryByTestId('advice-readonly')).toBeNull();
  });
});

describe('catalog matching (tier 2)', () => {
  it('asks the backend once and shows the matched tables', async () => {
    const table = { name: 'orders', schema: 'public' };
    mount(seed(ADVICE, {}, report(ADVICE, { checked: [table, { name: 'customers' }] })));
    await waitFor(() =>
      expect(screen.getAllByTestId('advice-status')[0]).toHaveAttribute('data-status', 'checked'),
    );
    expect(screen.getAllByTestId('advice-status')[0]).toHaveTextContent(
      'Matched against the indexes of 2 tables',
    );
    expect(advise).toHaveBeenCalledTimes(1);
    expect(advise).toHaveBeenCalledWith(
      's1',
      null,
      expect.objectContaining({ format: 'postgresJson' }),
      null,
      null,
      true,
    );
  });

  it('shows the checking state while waiting', async () => {
    let resolve!: (r: AdviceReport) => void;
    mount(seed(ADVICE, {}, () => new Promise<AdviceReport>((r) => (resolve = r))));
    await waitFor(() =>
      expect(screen.getAllByTestId('advice-status')[0]).toHaveAttribute('data-status', 'checking'),
    );
    expect(screen.getAllByTestId('advice-status')[0]).toHaveTextContent(
      'Checking existing indexes…',
    );
    // The tier-1 advice is already usable.
    expect(cardIds()).toHaveLength(3);
    await act(async () => resolve(report(ADVICE)));
    await waitFor(() =>
      expect(screen.getAllByTestId('advice-status')[0]).toHaveAttribute('data-status', 'checked'),
    );
  });

  it('replaces the advice with the matched one', async () => {
    const better = makeAdvice({
      id: 'indexForFilter:aaaa',
      confidence: 'high',
      nodeIds: [3],
      fingerprint: 'idx',
      params: {
        table: 'orders',
        columns: 'status, created',
        examined: '1',
        kept: '1',
        selectivity: '1%',
      },
    });
    mount(seed(ADVICE, {}, report([better])));
    await waitFor(() => expect(cardIds()).toEqual(['indexForFilter:aaaa']));
    expect(screen.getByRole('tab', { name: 'Advice (1)' })).toBeInTheDocument();
  });

  it('shows a partial match with the failed tables in the tooltip', async () => {
    mount(
      seed(
        ADVICE,
        {},
        report(ADVICE, {
          state: 'partial',
          failed: [{ table: 'customers', reason: 'timeout', message: 'slow' }],
        }),
      ),
    );
    await waitFor(() =>
      expect(screen.getAllByTestId('advice-status')[0]).toHaveAttribute('data-status', 'partial'),
    );
    expect(screen.getAllByTestId('advice-status')[0]).toHaveAttribute(
      'title',
      'customers: timeout',
    );
    expect(screen.getAllByTestId('advice-status')[0]).toHaveTextContent('Only partly matched');
  });

  it('keeps the first-tier advice and says so when the call fails', async () => {
    mount(seed(ADVICE, {}, () => Promise.reject({ kind: 'notFound', message: 'session gone' })));
    await waitFor(() =>
      expect(screen.getAllByTestId('advice-status')[0]).toHaveAttribute('data-status', 'failed'),
    );
    expect(screen.getAllByTestId('advice-status')[0]).toHaveAttribute('title', 'session gone');
    expect(cardIds()).toHaveLength(3);
  });

  it('does not call the backend when matching is switched off', async () => {
    vi.spyOn(ipc, 'settingsGet').mockResolvedValue({
      ...DEFAULT_SETTINGS,
      planAdvisorMetadata: false,
    });
    mount(seed());
    await waitFor(() =>
      expect(screen.getAllByTestId('advice-status')[0]).toHaveAttribute('data-status', 'disabled'),
    );
    expect(screen.getAllByTestId('advice-status')[0]).toHaveTextContent(
      'Matching against existing indexes is off',
    );
    expect(advise).not.toHaveBeenCalled();
  });

  it('reports an unavailable catalog', async () => {
    mount(seed(ADVICE, {}, report(ADVICE, { state: 'unavailable' })));
    await waitFor(() =>
      expect(screen.getAllByTestId('advice-status')[0]).toHaveAttribute(
        'data-status',
        'unavailable',
      ),
    );
  });
});

describe('statement-only advice', () => {
  it('highlights the range of the statement', async () => {
    const star = makeAdvice({
      id: 'star:1',
      rule: 'selectStar',
      nodeIds: [],
      statementRange: { start: 7, end: 8 },
      fingerprint: 'star',
    });
    const user = mount(seed([star], { statement: 'select * from orders' }));
    const card = await screen.findByTestId('advice-card-star:1');
    await user.click(within(card).getAllByRole('button')[0]!);
    const pre = within(card).getByTestId('advice-statement');
    expect(pre).toHaveTextContent('select * from orders');
    expect(pre.querySelector('mark')).toHaveTextContent('*');
  });
});

describe('compare with advice', () => {
  it('lists resolved, new and remaining advice between two plans', async () => {
    const a = seed([ADVICE[0]!, ADVICE[1]!]);
    const bAdvice = [
      { ...ADVICE[1]!, id: 'staleStatistics:zzzz' },
      makeAdvice({
        id: 'new:1',
        rule: 'largeOffset',
        nodeIds: [],
        params: { offset: '50000' },
        fingerprint: 'offset',
      }),
    ];
    seed(bAdvice, { statement: 'select 2' });
    render(
      <Providers>
        <PlanCompare planId={a} open onOpenChange={() => {}} />
      </Providers>,
    );
    const section = await screen.findByTestId('plan-compare-advice');
    const group = (g: string) =>
      within(section.querySelector(`[data-group="${g}"]`) as HTMLElement).getAllByTestId(
        'compare-advice-item',
      );
    expect(group('gone')).toHaveLength(1);
    expect(group('gone')[0]).toHaveTextContent('Add an index for the filter on orders');
    expect(group('new')[0]).toHaveTextContent('A large OFFSET reads rows it throws away');
    expect(group('kept')[0]).toHaveTextContent('Statistics on customers look out of date');
    const totals = within(screen.getByTestId('plan-compare-totals'));
    expect(totals.getByText('Advice')).toBeInTheDocument();
  });
});

describe('Vietnamese', () => {
  it('shows the summary in Vietnamese', async () => {
    await i18n.changeLanguage('vi');
    mount(seed());
    expect(await screen.findByTestId('advice-summary-text')).toHaveTextContent(
      '3 điểm tối ưu · lớn nhất: Thêm index cho điều kiện lọc trên orders — node chiếm ~78% thời gian',
    );
  });
});
