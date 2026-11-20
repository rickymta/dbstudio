import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { mockEmit } from '@/lib/mock/events';
import type { SessionStateEvent } from '@/lib/types';
import { useSessionHealthStore } from '@/stores/sessionHealth';
import { useSessionsStore } from '@/stores/sessions';
import { useTabsStore } from '@/stores/tabs';
import { SessionHealthBanner } from './SessionHealthBanner';

const event = (patch: Partial<SessionStateEvent>): SessionStateEvent => ({
  sessionId: 's1',
  connectionId: 'c1',
  state: 'lost',
  attempt: 0,
  error: null,
  leasesBroken: [],
  leasesReopened: [],
  ...patch,
});

async function setup() {
  const user = userEvent.setup();
  render(
    <Providers>
      <SessionHealthBanner />
    </Providers>,
  );
  // The listener is registered asynchronously.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
  return user;
}

const emit = (patch: Partial<SessionStateEvent>) =>
  act(() => mockEmit('session://state', event(patch)));

beforeEach(async () => {
  await i18n.changeLanguage('en');
  useSessionHealthStore.setState({ byConnection: {} });
  useTabsStore.setState({
    tabs: [{ id: 't1', kind: 'editor', title: 'q', payload: { connectionId: 'c1' } }],
    activeId: 't1',
  } as never);
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe('SessionHealthBanner', () => {
  it('shows nothing while the connection is healthy', async () => {
    await setup();
    expect(screen.queryByTestId('session-health')).toBeNull();
  });

  it('follows lost, reconnecting and reconnected, reporting what was lost', async () => {
    await setup();
    emit({ state: 'lost', leasesBroken: ['l1'] });
    expect(screen.getByRole('alert')).toHaveTextContent('was lost');
    expect(screen.getByRole('alert')).toHaveTextContent('One open transaction was lost');
    emit({ state: 'reconnecting', attempt: 2 });
    expect(screen.getByRole('alert')).toHaveTextContent('attempt 2');
    emit({ state: 'reconnected', leasesReopened: ['l2', 'l3'] });
    const status = screen.getByRole('status');
    expect(status).toHaveTextContent('Reconnected');
    expect(status).toHaveTextContent('One open transaction was lost');
    expect(status).toHaveTextContent('2 query tabs got a new server session');
  });

  it('removes a lossless reconnect notice by itself', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    await setup();
    emit({ state: 'reconnected' });
    expect(screen.getByRole('status')).toBeInTheDocument();
    act(() => vi.advanceTimersByTime(5100));
    expect(screen.queryByTestId('session-health')).toBeNull();
  });

  it('ignores other connections and clears on connected', async () => {
    await setup();
    emit({ connectionId: 'other', state: 'lost' });
    expect(screen.queryByTestId('session-health')).toBeNull();
    emit({ state: 'lost' });
    expect(screen.getByTestId('session-health')).toBeInTheDocument();
    emit({ state: 'connected' });
    expect(screen.queryByTestId('session-health')).toBeNull();
  });

  it('offers a manual reconnect after giving up', async () => {
    const reconnect = vi.spyOn(ipc, 'sessionReconnect').mockResolvedValue({
      sessionId: 's1',
      connectionId: 'c1',
    } as never);
    const user = await setup();
    emit({ state: 'gaveUp', attempt: 5, error: { kind: 'connection', message: 'refused' } });
    expect(screen.getByRole('alert')).toHaveTextContent('Connection failed: refused');
    await user.click(screen.getByRole('button', { name: 'Reconnect' }));
    await waitFor(() => expect(screen.queryByTestId('session-health')).toBeNull());
    expect(reconnect).toHaveBeenCalledWith('s1');
    expect(useSessionsStore.getState().sessionsByConnection.c1?.sessionId).toBe('s1');
  });

  it('keeps the banner and shows why a manual reconnect failed', async () => {
    vi.spyOn(ipc, 'sessionReconnect').mockRejectedValue({ kind: 'timeout', message: 'no route' });
    const user = await setup();
    emit({ state: 'gaveUp' });
    await user.click(screen.getByRole('button', { name: 'Reconnect' }));
    expect(await screen.findByText('Timed out: no route')).toBeInTheDocument();
    expect(screen.getByTestId('session-health')).toHaveAttribute('data-state', 'gaveUp');
  });
});
