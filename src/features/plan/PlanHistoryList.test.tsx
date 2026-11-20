import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import type { PlanHistoryEntry, RawPlan } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { useTabsStore, WELCOME_TAB } from '@/stores/tabs';
import { PlanHistoryList } from './PlanHistoryList';
import { usePlanStore } from './planStore';

const entry = (patch: Partial<PlanHistoryEntry>): PlanHistoryEntry => ({
  id: 'p1',
  connectionId: 'c1',
  createdAt: '2026-11-20T08:00:00Z',
  database: 'app_dev',
  statement: 'SELECT * FROM orders WHERE id = 1',
  analyze: false,
  format: 'postgresJson',
  totalCost: 12.5,
  totalMs: null,
  warnings: 0,
  ...patch,
});

const rawPlan = { statement: 'SELECT * FROM orders WHERE id = 1' } as RawPlan;

function mount(connected = true) {
  useSessionsStore.setState({
    sessionsByConnection: connected
      ? ({ c1: { sessionId: 's1', connectionId: 'c1' } } as never)
      : {},
  });
  render(
    <Providers>
      <PlanHistoryList connectionId="c1" />
    </Providers>,
  );
  return userEvent.setup();
}

beforeEach(async () => {
  await i18n.changeLanguage('en');
  usePlanStore.setState({ entries: {} });
  useTabsStore.setState({ tabs: [WELCOME_TAB], activeId: WELCOME_TAB.id });
});
afterEach(() => vi.restoreAllMocks());

describe('PlanHistoryList', () => {
  it('lists the plans of the connection with their figures', async () => {
    const list = vi
      .spyOn(ipc, 'planHistoryList')
      .mockResolvedValue([
        entry({}),
        entry({ id: 'p2', analyze: true, totalMs: 3.25, warnings: 2, statement: 'SELECT 2' }),
      ]);
    mount();
    const rows = await screen.findAllByTestId('plan-history-entry');
    expect(list).toHaveBeenCalledWith('c1', 50, null);
    expect(rows).toHaveLength(2);
    expect(rows[0]).toHaveTextContent('SELECT * FROM orders WHERE id = 1');
    expect(rows[0]).toHaveTextContent('cost 12.5');
    expect(rows[1]).toHaveTextContent('ANALYZE');
    expect(rows[1]).toHaveTextContent('2 warnings');
    expect(rows[1]).toHaveTextContent('3.25 ms');
  });

  it('says so when the history is empty', async () => {
    vi.spyOn(ipc, 'planHistoryList').mockResolvedValue([]);
    mount();
    expect(await screen.findByText('No plans in the history yet.')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Clear plan history' })).toBeDisabled();
  });

  it('reopens an entry as a plan tab', async () => {
    vi.spyOn(ipc, 'planHistoryList').mockResolvedValue([entry({})]);
    const get = vi.spyOn(ipc, 'planHistoryGet').mockResolvedValue({ ...entry({}), rawPlan });
    const user = mount();
    const row = await screen.findByTestId('plan-history-entry');
    await user.click(within(row).getByTitle('Open as a plan tab'));
    await waitFor(() =>
      expect(useTabsStore.getState().tabs.some((t) => t.kind === 'plan')).toBe(true),
    );
    expect(get).toHaveBeenCalledWith('p1');
    expect(Object.values(usePlanStore.getState().entries)[0]).toMatchObject({
      sessionId: 's1',
      connectionId: 'c1',
      statement: 'SELECT * FROM orders WHERE id = 1',
    });
  });

  it('does not open plans without a session', async () => {
    vi.spyOn(ipc, 'planHistoryList').mockResolvedValue([entry({})]);
    mount(false);
    const row = await screen.findByTestId('plan-history-entry');
    expect(screen.getByText('Connect to open a plan from the history.')).toBeInTheDocument();
    expect(within(row).getByTitle('Connect to open a plan from the history.')).toBeDisabled();
  });

  it('deletes one entry and clears all after confirmation', async () => {
    vi.spyOn(ipc, 'planHistoryList').mockResolvedValue([entry({})]);
    const del = vi.spyOn(ipc, 'planHistoryDelete').mockResolvedValue();
    const clear = vi.spyOn(ipc, 'planHistoryClear').mockResolvedValue(1);
    const user = mount();
    const row = await screen.findByTestId('plan-history-entry');
    await user.click(within(row).getByRole('button', { name: 'Delete from the plan history' }));
    expect(del).toHaveBeenCalledWith('p1');
    await user.click(screen.getByRole('button', { name: 'Clear plan history' }));
    const dialog = await screen.findByRole('dialog', { name: 'Clear the plan history?' });
    await user.click(within(dialog).getByRole('button', { name: 'Clear plan history' }));
    await waitFor(() => expect(clear).toHaveBeenCalledWith('c1'));
  });
});
