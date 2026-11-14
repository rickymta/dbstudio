import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { DEFAULT_SETTINGS } from '@/lib/types';
import type { ConnectionProfile, PlanNode, PlanResult, SessionInfo } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { useTabsStore, WELCOME_TAB } from '@/stores/tabs';
import { polyfillLayoutApis } from '../data/testUtils';
import { openRawPlan } from './open';
import { useAdviceStore } from './PlanAdviceData';
import { PlanTab } from './PlanTab';
import { usePlanStore } from './planStore';
import { rawOf, SAMPLES, seedPlan, TREES } from './planTestData';

vi.mock('./PlanGraph', () => ({ PlanGraph: () => <div data-testid="plan-graph" /> }));
vi.mock('./elkLayout', () => ({
  NODE_WIDTH: 240,
  NODE_HEIGHT: 76,
  layoutPlan: async (root: PlanNode) => ({
    positions: new Map([[root.id, { x: 0, y: 0 }]]),
    width: 100,
    height: 100,
  }),
}));

beforeAll(() => polyfillLayoutApis());
beforeEach(async () => {
  usePlanStore.setState({ entries: {} });
  useAdviceStore.setState({ slots: {} });
  useTabsStore.setState({ tabs: [WELCOME_TAB], activeId: WELCOME_TAB.id });
  useSessionsStore.setState({ sessionsByConnection: {} });
  await i18n.changeLanguage('en');
  vi.spyOn(ipc, 'settingsGet').mockResolvedValue({ ...DEFAULT_SETTINGS });
});
afterEach(() => vi.restoreAllMocks());

const report = {
  advice: [],
  metadata: { state: 'checked', checked: [], failed: [], skipped: 0, elapsedMs: 1 },
};

describe('plans that do not come from the open connection', () => {
  it('a plan opened from a file never asks for catalog metadata', async () => {
    vi.spyOn(ipc, 'planParse').mockResolvedValue(structuredClone(TREES[SAMPLES.pg.file]!));
    const advise = vi
      .spyOn(ipc, 'planAdvise')
      .mockResolvedValue(report as unknown as Awaited<ReturnType<typeof ipc.planAdvise>>);
    const id = openRawPlan(
      { sessionId: 's1', connectionId: 'c1' },
      rawOf(SAMPLES.pg.format, SAMPLES.pg.text),
    );
    expect(usePlanStore.getState().entries[id]!.catalog).toBe(false);
    render(
      <Providers>
        <PlanTab planId={id} sessionId="s1" connectionId="c1" />
      </Providers>,
    );
    await waitFor(() => expect(advise).toHaveBeenCalled());
    // `useCatalog` is the sixth argument.
    expect(advise.mock.calls[0]![5]).toBe(false);
  });

  it('a plan explained in the tab does', async () => {
    const advise = vi
      .spyOn(ipc, 'planAdvise')
      .mockResolvedValue(report as unknown as Awaited<ReturnType<typeof ipc.planAdvise>>);
    const id = seedPlan();
    render(
      <Providers>
        <PlanTab planId={id} sessionId="s1" connectionId="c1" />
      </Providers>,
    );
    await waitFor(() => expect(advise).toHaveBeenCalled());
    expect(advise.mock.calls[0]![5]).toBe(true);
  });
});

describe('reconnecting a lost plan session', () => {
  const info = { sessionId: 's2', connectionId: 'c1' } as SessionInfo;
  const lost = { kind: 'notFound' as const, message: 'session s1' };
  const profile = {
    id: 'c1',
    name: 'Shop',
    kind: 'postgres',
    savePassword: true,
    hasPassword: true,
  } as ConnectionProfile;

  function mount(id: string) {
    vi.spyOn(ipc, 'sessionReconnect').mockResolvedValue(info);
    vi.spyOn(ipc, 'connectionsList').mockResolvedValue({ groups: [], profiles: [profile] });
    render(
      <Providers>
        <PlanTab planId={id} sessionId="s1" connectionId="c1" />
      </Providers>,
    );
  }

  it('never repeats an ANALYZE on its own', async () => {
    const explain = vi.spyOn(ipc, 'planExplain').mockResolvedValue({
      raw: rawOf(SAMPLES.pg.format, SAMPLES.pg.text),
      tree: structuredClone(TREES[SAMPLES.pg.file]!),
      parseError: null,
    } satisfies PlanResult);
    const id = seedPlan({ status: 'error', tree: null, raw: null, error: lost, leaseId: 'l1' });
    mount(id);
    const user = userEvent.setup();
    await act(async () => {
      await user.click(screen.getByRole('button', { name: 'Reconnect' }));
    });
    expect(await screen.findByText('Reconnected')).toBeInTheDocument();
    expect(explain).not.toHaveBeenCalled();
    const e = usePlanStore.getState().entries[id]!;
    expect(e.sessionId).toBe('s2');
    expect(e.leaseId).toBeNull();
    // The user can run it again (the guard for writes applies as for the toolbar button).
    expect(screen.getAllByRole('button', { name: 'Run again' }).length).toBeGreaterThan(0);
  });

  it('repeats an estimate right away', async () => {
    const explain = vi.spyOn(ipc, 'planExplain').mockResolvedValue({
      raw: rawOf(SAMPLES.pg.format, SAMPLES.pg.text),
      tree: structuredClone(TREES[SAMPLES.pg.file]!),
      parseError: null,
    } satisfies PlanResult);
    const id = seedPlan({ status: 'error', tree: null, raw: null, error: lost });
    const request = usePlanStore.getState().entries[id]!.request!;
    usePlanStore.getState().patch(id, { request: { ...request, analyze: false } });
    mount(id);
    const user = userEvent.setup();
    await act(async () => {
      await user.click(screen.getByRole('button', { name: 'Reconnect' }));
    });
    await waitFor(() => expect(explain).toHaveBeenCalledTimes(1));
    expect(explain.mock.calls[0]![0]).toBe('s2');
  });
});
