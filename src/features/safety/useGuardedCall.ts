import { useCallback } from 'react';
import { toIpcError } from '@/lib/ipc';
import type { ConfirmationGrant, ConfirmationRequest } from '@/lib/types';
import { askConfirmation } from './confirmStore';

/** A backend call that accepts the grant answering a `confirmationRequired` error. */
export type GuardedCall<T> = (confirm?: ConfirmationGrant) => Promise<T>;

/** A token can expire while the user types; ask again a few times before giving up. */
const MAX_ASKS = 3;

/**
 * Run `call`; when it fails with `confirmationRequired`, ask the user (typing the confirmation
 * text) and call again with the grant. Declining rejects with a `cancelled` error. Every other
 * error is rethrown as an `IpcError`.
 */
export async function guardedCall<T>(
  call: GuardedCall<T>,
  ask: (request: ConfirmationRequest) => Promise<ConfirmationGrant> = askConfirmation,
): Promise<T> {
  let grant: ConfirmationGrant | undefined;
  for (let asked = 0; ; asked++) {
    try {
      return await call(grant);
    } catch (e) {
      const err = toIpcError(e);
      if (err.kind !== 'confirmationRequired' || !err.confirmation || asked >= MAX_ASKS) throw err;
      grant = await ask(err.confirmation);
    }
  }
}

/**
 * Wrapper for calls that may need confirmation on production connections: catches
 * `confirmationRequired`, asks through `ConfirmDestructiveDialog` (rendered by
 * `ConfirmDestructiveHost`) and calls again with the grant.
 */
export function useGuardedCall(): <T>(call: GuardedCall<T>) => Promise<T> {
  return useCallback((call) => guardedCall(call), []);
}
