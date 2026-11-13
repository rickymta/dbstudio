import { useMemo } from 'react';
import type { ObjectNode, SessionInfo } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';

export const SERVER_SEARCH_MIN_CHARS = 2;
export const SERVER_SEARCH_LIMIT = 50;

/** Open sessions whose driver can search on the server (capability-driven, no DB names). */
export function useSearchableSessions(): SessionInfo[] {
  const sessions = useSessionsStore((s) => s.sessionsByConnection);
  return useMemo(
    () => Object.values(sessions).filter((s) => s.descriptor.capabilities.objectSearch),
    [sessions],
  );
}

/** Display context of a result: its path without the folder segments and the object itself. */
export function resultContext(node: ObjectNode): string {
  return node.path.segments
    .slice(0, -1)
    .filter((s) => s.kind !== 'folder')
    .map((s) => s.name)
    .join('.');
}
