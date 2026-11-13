import { useEffect, useRef, useState } from 'react';
import { ipc, toIpcError } from '@/lib/ipc';
import { ROOT_PATH } from '@/lib/objectPath';
import type { IpcError, ObjectNode, SessionInfo, Uuid } from '@/lib/types';
import { SERVER_SEARCH_LIMIT } from './serverSearchModel';

export interface ServerSearchState {
  /** A request for the current text is pending or waiting for the running one to finish. */
  fetching: boolean;
  data?: ObjectNode[];
  error?: IpcError;
}

interface Outcome {
  query: string;
  data?: ObjectNode[];
  error?: IpcError;
}

interface Slot {
  running: boolean;
  /** Newest requested text; `''` means nothing should run. */
  latest: string;
}

/**
 * Run `object_search` on every given session, at most ONE request per session at a time. The
 * contract has no way to cancel a search, so while one runs, text changes only record the newest
 * text; when the running search ends, its answer is dropped if the text moved on and just the
 * newest text is sent. Intermediate texts are never sent.
 */
export function useServerSearch(
  sessions: SessionInfo[],
  query: string,
  enabled: boolean,
): ServerSearchState[] {
  const slots = useRef(new Map<Uuid, Slot>());
  const [outcomes, setOutcomes] = useState<Record<Uuid, Outcome>>({});

  useEffect(() => {
    const drain = async (session: SessionInfo, slot: Slot) => {
      slot.running = true;
      try {
        while (slot.latest) {
          const text = slot.latest;
          let outcome: Outcome;
          try {
            const data = await ipc.objectSearch(session.sessionId, {
              query: text,
              scope: ROOT_PATH,
              limit: SERVER_SEARCH_LIMIT,
            });
            outcome = { query: text, data };
          } catch (e) {
            outcome = { query: text, error: toIpcError(e) };
          }
          if (slot.latest === text) {
            setOutcomes((prev) => ({ ...prev, [session.sessionId]: outcome }));
            return;
          }
          // Stale answer: ignore it and search the newest text instead.
        }
      } finally {
        slot.running = false;
      }
    };

    for (const session of sessions) {
      let slot = slots.current.get(session.sessionId);
      if (!slot) {
        slot = { running: false, latest: '' };
        slots.current.set(session.sessionId, slot);
      }
      slot.latest = enabled ? query : '';
      if (slot.latest && !slot.running) void drain(session, slot);
    }
  }, [sessions, query, enabled]);

  return sessions.map((s) => {
    const outcome = outcomes[s.sessionId];
    if (!enabled) return { fetching: false };
    if (outcome && outcome.query === query) {
      return { fetching: false, data: outcome.data, error: outcome.error };
    }
    return { fetching: true };
  });
}
