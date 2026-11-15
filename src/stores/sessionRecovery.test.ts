import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ipc } from '@/lib/ipc';
import type { ConnectionProfile, SessionInfo } from '@/lib/types';
import { isPasswordRequired, reconnectSession } from './sessionRecovery';
import { useSessionsStore } from './sessions';

const stale = { sessionId: 'old', connectionId: 'c1' } as SessionInfo;
const fresh = { sessionId: 'new', connectionId: 'c1' } as SessionInfo;
const lost = { kind: 'notFound', message: 'session old' };
const profile = (over: Partial<ConnectionProfile>) =>
  ({
    id: 'c1',
    name: 'Shop',
    kind: 'postgres',
    savePassword: true,
    hasPassword: true,
    ...over,
  }) as ConnectionProfile;

beforeEach(() => {
  useSessionsStore.setState({ sessionsByConnection: { c1: stale }, opening: {} });
  vi.spyOn(ipc, 'sessionReconnect').mockRejectedValue(lost);
});
afterEach(() => vi.restoreAllMocks());

describe('reconnectSession fallback', () => {
  it('opens a fresh session and replaces the stale entry once it is open', async () => {
    vi.spyOn(ipc, 'connectionsList').mockResolvedValue({ groups: [], profiles: [profile({})] });
    const open = vi.spyOn(ipc, 'sessionOpen').mockResolvedValue(fresh);
    await expect(reconnectSession('old', 'c1')).resolves.toBe(fresh);
    expect(open).toHaveBeenCalledWith('c1', null);
    expect(useSessionsStore.getState().sessionsByConnection.c1).toBe(fresh);
  });

  it('keeps the stale entry when opening fails', async () => {
    vi.spyOn(ipc, 'connectionsList').mockResolvedValue({ groups: [], profiles: [profile({})] });
    vi.spyOn(ipc, 'sessionOpen').mockRejectedValue({ kind: 'sshHostKey', message: 'host key' });
    await expect(reconnectSession('old', 'c1')).rejects.toMatchObject({ kind: 'sshHostKey' });
    expect(useSessionsStore.getState().sessionsByConnection.c1).toBe(stale);
  });

  it('never asks for a password when the profile has no authentication', async () => {
    vi.spyOn(ipc, 'connectionsList').mockResolvedValue({
      groups: [],
      profiles: [profile({ savePassword: false, hasPassword: false, authMode: 'none' })],
    });
    const open = vi.spyOn(ipc, 'sessionOpen').mockResolvedValue(fresh);
    await expect(reconnectSession('old', 'c1')).resolves.toBe(fresh);
    expect(open).toHaveBeenCalledWith('c1', null);
  });

  it('asks for the password of a profile that stores none, without touching the entry', async () => {
    vi.spyOn(ipc, 'connectionsList').mockResolvedValue({
      groups: [],
      profiles: [profile({ savePassword: false })],
    });
    const open = vi.spyOn(ipc, 'sessionOpen').mockResolvedValue(fresh);
    const err = await reconnectSession('old', 'c1').catch((e: unknown) => e);
    expect(isPasswordRequired(err)).toBe(true);
    expect(open).not.toHaveBeenCalled();
    expect(useSessionsStore.getState().sessionsByConnection.c1).toBe(stale);
    // With the password the session opens.
    await expect(reconnectSession('old', 'c1', { password: 'pw' })).resolves.toBe(fresh);
    expect(open).toHaveBeenCalledWith('c1', 'pw');
  });
});
