import { create } from 'zustand';
import { ipc } from '@/lib/ipc';
import type { IpcError, SessionInfo, Uuid } from '@/lib/types';
import { useTabsStore } from './tabs';

interface SessionsState {
  /** Open sessions keyed by connection (profile) id; at most one per connection. */
  sessionsByConnection: Record<Uuid, SessionInfo>;
  /** Connection ids with a `session_open` in flight. */
  opening: Record<Uuid, true>;
  /** Open (or reuse) the session of a connection. Rejects with an `IpcError`. */
  open: (connectionId: Uuid, password?: string | null) => Promise<SessionInfo>;
  /** Close the session of a connection and every tab that belongs to it. */
  close: (connectionId: Uuid) => Promise<void>;
}

function omit<V>(record: Record<string, V>, key: string): Record<string, V> {
  const copy = { ...record };
  delete copy[key];
  return copy;
}

const inFlight = new Map<Uuid, Promise<SessionInfo>>();
/** Connections whose in-flight `open` was cancelled by `close`; the session is closed on arrival. */
const cancelled = new Set<Uuid>();

export const useSessionsStore = create<SessionsState>((set, get) => ({
  sessionsByConnection: {},
  opening: {},
  open: (connectionId, password) => {
    const existing = get().sessionsByConnection[connectionId];
    if (existing) return Promise.resolve(existing);
    const pending = inFlight.get(connectionId);
    if (pending) {
      // Asking again while a cancelled open is still in flight revives it.
      cancelled.delete(connectionId);
      return pending;
    }
    set((s) => ({ opening: { ...s.opening, [connectionId]: true } }));
    const promise = ipc
      .sessionOpen(connectionId, password)
      .then((info) => {
        if (cancelled.delete(connectionId)) {
          ipc.sessionClose(info.sessionId).catch(() => undefined);
          throw { kind: 'cancelled', message: 'connection cancelled' } satisfies IpcError;
        }
        set((s) => ({ sessionsByConnection: { ...s.sessionsByConnection, [connectionId]: info } }));
        return info;
      })
      .finally(() => {
        inFlight.delete(connectionId);
        set((s) => ({ opening: omit(s.opening, connectionId) }));
      });
    inFlight.set(connectionId, promise);
    return promise;
  },
  close: async (connectionId) => {
    const info = get().sessionsByConnection[connectionId];
    if (!info) {
      if (inFlight.has(connectionId)) {
        cancelled.add(connectionId);
        set((st) => ({ opening: omit(st.opening, connectionId) }));
      }
      return;
    }
    set((s) => ({ sessionsByConnection: omit(s.sessionsByConnection, connectionId) }));
    useTabsStore.getState().closeSessionTabs(info.sessionId);
    try {
      await ipc.sessionClose(info.sessionId);
    } catch {
      // The session is gone from the UI either way.
    }
  },
}));

/** Session of a connection, or `undefined` when not connected. */
export const useSession = (connectionId: Uuid): SessionInfo | undefined =>
  useSessionsStore((s) => s.sessionsByConnection[connectionId]);

/** True while the session of a connection is being opened. */
export const useIsOpening = (connectionId: Uuid): boolean =>
  useSessionsStore((s) => s.opening[connectionId] === true);

/** Connection id owning `sessionId` (non-reactive helper). */
export function connectionIdOfSession(sessionId: Uuid): Uuid | undefined {
  return Object.values(useSessionsStore.getState().sessionsByConnection).find(
    (s) => s.sessionId === sessionId,
  )?.connectionId;
}
