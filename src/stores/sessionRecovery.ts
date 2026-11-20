import { ipc } from '@/lib/ipc';
import type { IpcError, SessionInfo, Uuid } from '@/lib/types';
import { needsPassword } from '@/features/connections/connectionsTree';
import { useSessionsStore } from './sessions';

/** True for the error of a call on a session the backend no longer knows. */
export function isSessionLost(e: unknown): boolean {
  return typeof e === 'object' && e !== null && (e as { kind?: unknown }).kind === 'notFound';
}

/** Marker of the error of a reconnect that needs the password of the profile. */
const PASSWORD_REQUIRED = 'passwordRequired';

/** True when [`reconnectSession`] stopped because the profile stores no password. */
export function isPasswordRequired(e: unknown): boolean {
  return (
    typeof e === 'object' &&
    e !== null &&
    (e as { kind?: unknown }).kind === 'authentication' &&
    (e as { capability?: unknown }).capability === PASSWORD_REQUIRED
  );
}

/**
 * Bring a lost session back. First `session_reconnect` (same session id); when the backend has
 * forgotten the session altogether (`notFound`), open a fresh session of the connection the way
 * the connections list does: a profile without a stored password needs `opts.password` (the
 * call rejects with a password-required error to ask for it, see [`isPasswordRequired`]) and an
 * unknown SSH host key rejects with `sshHostKey` for the caller to run through
 * `useHostKeyPrompt`. The stale entry of the sessions store is replaced once the new session is
 * open; when opening fails it stays as it was.
 */
export async function reconnectSession(
  sessionId: Uuid,
  connectionId: Uuid,
  opts: { password?: string | null } = {},
): Promise<SessionInfo> {
  try {
    // The password typed for this attempt also serves a reconnect of the same session.
    const info =
      opts.password == null
        ? await ipc.sessionReconnect(sessionId)
        : await ipc.sessionReconnect(sessionId, opts.password);
    useSessionsStore.setState((s) => ({
      sessionsByConnection: { ...s.sessionsByConnection, [connectionId]: info },
    }));
    return info;
  } catch (e) {
    if (!isSessionLost(e)) throw e;
    if (opts.password == null) {
      const snapshot = await ipc.connectionsList();
      const profile = snapshot.profiles.find((p) => p.id === connectionId);
      if (profile && needsPassword(profile)) {
        throw {
          kind: 'authentication',
          message: 'The password of this connection is needed to open a new session.',
          capability: PASSWORD_REQUIRED,
        } satisfies IpcError;
      }
    }
    return useSessionsStore.getState().open(connectionId, opts.password ?? null, { replace: true });
  }
}
