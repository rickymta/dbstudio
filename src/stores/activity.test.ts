import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  ACTIVITY_RETENTION_MS,
  activityStorage,
  resetActivity,
  trackCommand,
  useActivity,
} from './activity';

const items = () => useActivity.getState().items;

beforeEach(() => resetActivity());
afterEach(() => vi.useRealTimers());

describe('activity log', () => {
  it('logs started and finished with the duration', async () => {
    await trackCommand('session_open', { connectionId: 'c1' }, async () => ({
      sessionId: 's1',
      connectionId: 'c1',
    }));
    expect(items().map((i) => [i.connectionId, i.event, i.status])).toEqual([
      ['c1', 'connect', 'started'],
      ['c1', 'connect', 'finished'],
    ]);
    expect(items()[1]!.ms).toBeTypeOf('number');
  });

  it('logs the error of a failed command and rethrows it', async () => {
    const failure = { kind: 'connection', message: 'refused' };
    await expect(
      trackCommand('session_open', { connectionId: 'c1' }, () => Promise.reject(failure)),
    ).rejects.toBe(failure);
    expect(items().at(-1)).toMatchObject({ status: 'failed', error: 'refused' });
  });

  it('does not show a cancelled prompt as a failure', async () => {
    await expect(
      trackCommand('session_open', { connectionId: 'c1' }, () =>
        Promise.reject({ kind: 'cancelled', message: 'cancelled' }),
      ),
    ).rejects.toMatchObject({ kind: 'cancelled' });
    expect(items().at(-1)).toMatchObject({ status: 'finished' });
  });

  it('ignores commands that are not tracked', async () => {
    await trackCommand('object_children', { sessionId: 's1' }, async () => []);
    expect(items()).toEqual([]);
  });

  it('files session and lease commands under the connection it learnt', async () => {
    await trackCommand('session_open', { connectionId: 'c1' }, async () => ({
      sessionId: 's1',
      connectionId: 'c1',
    }));
    await trackCommand('lease_open', { sessionId: 's1' }, async () => ({
      leaseId: 'l1',
      sessionId: 's1',
    }));
    await trackCommand('lease_commit', { leaseId: 'l1' }, async () => ({}));
    await trackCommand('changes_apply', { sessionId: 's1' }, async () => ({}));
    await trackCommand('settings_set', { settings: {} }, async () => ({}));
    const last = (event: string) =>
      [...items()].reverse().find((i) => i.event === event)?.connectionId;
    expect(last('commit')).toBe('c1');
    expect(last('applyChanges')).toBe('c1');
    expect(last('saveSettings')).toBeNull();
  });

  it('never stores arguments (statements, passwords)', async () => {
    await trackCommand(
      'lease_execute',
      { leaseId: 'l1', request: { text: 'select secret_value' }, password: 'hunter2' },
      async () => ({}),
    );
    const dump = JSON.stringify(items());
    expect(dump).not.toContain('secret_value');
    expect(dump).not.toContain('hunter2');
  });

  it('clears one connection, the application entries or everything', () => {
    const { log, clear } = useActivity.getState();
    log({ connectionId: 'c1', event: 'connect', status: 'info' });
    log({ connectionId: 'c2', event: 'connect', status: 'info' });
    log({ connectionId: null, event: 'saveSettings', status: 'info' });
    clear('c1');
    expect(items().map((i) => i.connectionId)).toEqual(['c2', null]);
    clear(null);
    expect(items().map((i) => i.connectionId)).toEqual(['c2']);
    clear('all');
    expect(items()).toEqual([]);
  });

  it('saves to localStorage after a moment and drops entries past the retention on load', () => {
    vi.useFakeTimers();
    const now = Date.now();
    useActivity.getState().log({ connectionId: null, event: 'saveSettings', status: 'info' });
    expect(localStorage.getItem(activityStorage.key)).toBeNull();
    vi.advanceTimersByTime(1000);
    expect(JSON.parse(localStorage.getItem(activityStorage.key)!)).toHaveLength(1);

    localStorage.setItem(
      activityStorage.key,
      JSON.stringify([
        { connectionId: null, event: 'old', status: 'info', at: now - ACTIVITY_RETENTION_MS - 1 },
        { connectionId: 'c1', event: 'fresh', status: 'info', at: now - 1000 },
        { broken: true },
      ]),
    );
    expect(activityStorage.load(now).map((i) => i.event)).toEqual(['fresh']);
  });
});
