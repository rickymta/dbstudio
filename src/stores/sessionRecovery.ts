import { ipc } from '@/lib/ipc';
import type { SessionInfo, Uuid } from '@/lib/types';
import { useSessionsStore } from './sessions';

/** True for the error of a call on a session the backend no longer knows. */
export function isSessionLost(e: unknown): boolean {
  return typeof e === 'object' && e !== null && (e as { kind?: unknown }).kind === 'notFound';
}

/**
 * Bring a lost session back. First `session_reconnect` (same session id); when the backend has
 * forgotten the session altogether (`notFound`), drop the stale entry and open a fresh session
 * of the connection. The sessions store is updated either way.
 */
export async function reconnectSession(sessionId: Uuid, connectionId: Uuid): Promise<SessionInfo> {
  try {
    const info = await ipc.sessionReconnect(sessionId);
    useSessionsStore.setState((s) => ({
      sessionsByConnection: { ...s.sessionsByConnection, [connectionId]: info },
    }));
    return info;
  } catch (e) {
    if (!isSessionLost(e)) throw e;
    useSessionsStore.setState((s) => {
      const copy = { ...s.sessionsByConnection };
      delete copy[connectionId];
      return { sessionsByConnection: copy };
    });
    return useSessionsStore.getState().open(connectionId);
  }
}
