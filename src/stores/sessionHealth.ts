import { create } from 'zustand';
import { EVENTS, ipc } from '@/lib/ipc';
import type { IpcError, SessionHealth, SessionStateEvent, Uuid } from '@/lib/types';

/** What the backend last reported about the session of a connection (`session://state`). */
export interface ConnectionHealth {
  sessionId: Uuid;
  state: Exclude<SessionHealth, 'connected'>;
  /** Reconnect attempt number while `reconnecting`. */
  attempt: number;
  error: IpcError | null;
  /** Leases whose transaction was lost with the connection. */
  leasesBroken: number;
  /** Idle leases reopened under the same id: their session state (SET, temp tables) is gone. */
  leasesReopened: number;
}

interface SessionHealthState {
  /** Only connections that are not plainly connected have an entry. */
  byConnection: Record<Uuid, ConnectionHealth>;
  apply: (event: SessionStateEvent) => void;
  /** Forget the entry (the user dismissed the banner, or the session was replaced). */
  clear: (connectionId: Uuid) => void;
}

export const useSessionHealthStore = create<SessionHealthState>((set) => ({
  byConnection: {},
  apply: (event) =>
    set((s) => {
      if (event.state === 'connected')
        return { byConnection: omit(s.byConnection, event.connectionId) };
      const previous = s.byConnection[event.connectionId];
      // The lease lists come with the events of one outage; keep the largest seen until it ends.
      const carry = previous?.sessionId === event.sessionId && previous.state !== 'reconnected';
      return {
        byConnection: {
          ...s.byConnection,
          [event.connectionId]: {
            sessionId: event.sessionId,
            state: event.state,
            attempt: event.attempt,
            error: event.error,
            leasesBroken: Math.max(event.leasesBroken.length, carry ? previous.leasesBroken : 0),
            leasesReopened: Math.max(
              event.leasesReopened.length,
              carry ? previous.leasesReopened : 0,
            ),
          },
        },
      };
    }),
  clear: (connectionId) =>
    set((s) =>
      connectionId in s.byConnection ? { byConnection: omit(s.byConnection, connectionId) } : s,
    ),
}));

function omit<V>(record: Record<string, V>, key: string): Record<string, V> {
  const copy = { ...record };
  delete copy[key];
  return copy;
}

/** Subscribe the store to `session://state`; resolves to the function that unsubscribes. */
export function listenSessionHealth(): Promise<() => void> {
  return ipc.onEvent<SessionStateEvent>(EVENTS.sessionState, (event) =>
    useSessionHealthStore.getState().apply(event),
  );
}
