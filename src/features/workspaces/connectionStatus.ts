import { toast } from '@/components/ui/toastStore';
import { useConnectErrors } from '@/features/connections/connectErrors';
import { needsPassword } from '@/features/connections/connectionsTree';
import i18n from '@/lib/i18n';
import { toIpcError } from '@/lib/ipc';
import type { ConnectionProfile, Uuid } from '@/lib/types';
import { useSessionHealthStore } from '@/stores/sessionHealth';
import { useSessionsStore } from '@/stores/sessions';

/** What a connection is doing right now, as the overview and the Launchpad show it. */
export type ConnectionStatus =
  'connected' | 'opening' | 'reconnecting' | 'lost' | 'gaveUp' | 'failed' | 'idle';

/** Statuses that need the user's attention. */
export const PROBLEM_STATUSES: ReadonlySet<ConnectionStatus> = new Set([
  'lost',
  'gaveUp',
  'failed',
]);

interface StatusInputs {
  sessions: Record<Uuid, unknown>;
  opening: Record<Uuid, true>;
  health: Record<Uuid, { state: string }>;
  errors: Record<Uuid, unknown>;
}

export function statusOf(id: Uuid, inputs: StatusInputs): ConnectionStatus {
  const health = inputs.health[id]?.state;
  if (inputs.sessions[id]) {
    if (health === 'reconnecting') return 'reconnecting';
    if (health === 'lost') return 'lost';
    if (health === 'gaveUp') return 'gaveUp';
    return 'connected';
  }
  if (inputs.opening[id]) return 'opening';
  if (inputs.errors[id]) return 'failed';
  return 'idle';
}

/** React hook: a function giving the status of a connection (re-renders when any changes). */
export function useConnectionStatus(): (id: Uuid) => ConnectionStatus {
  const sessions = useSessionsStore((s) => s.sessionsByConnection);
  const opening = useSessionsStore((s) => s.opening);
  const health = useSessionHealthStore((s) => s.byConnection);
  const errors = useConnectErrors((s) => s.byConnection);
  return (id) => statusOf(id, { sessions, opening, health, errors });
}

/**
 * Connect several profiles, four at a time. Profiles that need a password typed in, or whose
 * connect needs a question answered (an unknown SSH host key), are left for the sidebar: one
 * summary toast tells how many connected, were skipped or failed.
 */
export async function connectMany(profiles: ConnectionProfile[]): Promise<void> {
  const sessions = useSessionsStore.getState();
  const todo = profiles.filter((p) => !sessions.sessionsByConnection[p.id]);
  const skipped = todo.filter(needsPassword);
  const queue = todo.filter((p) => !needsPassword(p));
  let ok = 0;
  const failed: string[] = [];
  const worker = async () => {
    for (let p = queue.shift(); p; p = queue.shift()) {
      try {
        await useSessionsStore.getState().open(p.id, null);
        useConnectErrors.getState().clear(p.id);
        ok++;
      } catch (e) {
        const message = toIpcError(e).message;
        useConnectErrors.getState().set(p.id, message);
        failed.push(`${p.name}: ${message.split('\n')[0]}`);
      }
    }
  };
  await Promise.all(Array.from({ length: Math.min(4, queue.length) }, worker));
  const parts = [i18n.t('workspaces:bulk.connected', { count: ok })];
  if (skipped.length) parts.push(i18n.t('workspaces:bulk.needPassword', { count: skipped.length }));
  if (failed.length) {
    parts.push(i18n.t('workspaces:bulk.failed', { count: failed.length }));
    toast.error(`${parts.join(' · ')}\n${failed.slice(0, 3).join('\n')}`);
  } else if (todo.length) toast.success(parts.join(' · '));
}

/** Disconnect several connections; tabs with unfinished work may refuse (one dialog each). */
export async function disconnectMany(ids: Uuid[]): Promise<void> {
  for (const id of ids) {
    if (!useSessionsStore.getState().sessionsByConnection[id]) continue;
    if (!(await useSessionsStore.getState().requestClose(id))) return;
  }
}
