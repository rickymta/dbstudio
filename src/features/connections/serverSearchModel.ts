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

/** Names above a result: its path without folder segments and without the object itself. */
export function resultContextParts(node: Pick<ObjectNode, 'path'>): string[] {
  return node.path.segments
    .slice(0, -1)
    .filter((s) => s.kind !== 'folder')
    .map((s) => s.name);
}

/** Display context of a result, e.g. `db › schema`. */
export function resultContext(node: Pick<ObjectNode, 'path'>): string {
  return resultContextParts(node).join(' › ');
}

/**
 * The part of the backend `detail` that is not already said by the context. Drivers put the
 * location (`db.schema`, `schema`, a database name) first and may append more (`partitioned`).
 */
export function extraDetail(node: Pick<ObjectNode, 'path' | 'detail'>): string {
  let detail = (node.detail ?? '').trim();
  if (!detail) return '';
  const parts = resultContextParts(node);
  const known = [parts.join('.'), parts.join(' › '), parts.at(-1) ?? ''].filter(Boolean);
  for (const c of known) {
    if (!detail.toLowerCase().startsWith(c.toLowerCase())) continue;
    const rest = detail.slice(c.length);
    if (rest === '' || /^[\s,;·-]/.test(rest)) {
      detail = rest.replace(/^[\s,;·-]+/, '');
      break;
    }
  }
  return detail;
}

/**
 * Database a server search is limited to, or `undefined` when the driver searches the whole
 * server. PostgreSQL cannot look across databases from one connection, so it only sees the
 * database of the session.
 */
export function searchScopeDatabase(
  session: Pick<SessionInfo, 'descriptor' | 'serverInfo'>,
  profileDatabase?: string | null,
): string | undefined {
  if (session.descriptor.kind !== 'postgres') return undefined;
  return session.serverInfo.extra?.database || profileDatabase || undefined;
}
