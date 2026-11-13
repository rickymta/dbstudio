import { useQuery } from '@tanstack/react-query';
import { ipc } from '@/lib/ipc';
import { safeColor } from './color';
import type { ConnectionProfile, Uuid } from '@/lib/types';

/** Query key of the connections snapshot; invalidate it after every connection/group mutation. */
export const connectionsQueryKey = ['connections'] as const;

export function useConnectionsSnapshot() {
  return useQuery({ queryKey: connectionsQueryKey, queryFn: ipc.connectionsList });
}

/** Profile by id from the cached snapshot. */
export function useConnectionProfile(connectionId: Uuid): ConnectionProfile | undefined {
  const { data } = useConnectionsSnapshot();
  return data?.profiles.find((p) => p.id === connectionId);
}

/** Color to mark a connection with: its own color, else a hint from its environment. */
export function connectionColor(profile: ConnectionProfile | undefined): string | undefined {
  if (!profile) return undefined;
  const own = safeColor(profile.color);
  if (own) return own;
  switch (profile.environment) {
    case 'production':
      return 'var(--danger)';
    case 'staging':
      return 'var(--warning)';
    case 'development':
      return 'var(--success)';
    default:
      return undefined;
  }
}
