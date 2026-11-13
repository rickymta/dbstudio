import type { ConfirmationGrant } from '@/lib/types';

/** A backend call that accepts the grant answering a `confirmationRequired` error. */
export type GuardedCall<T> = (confirm?: ConfirmationGrant) => Promise<T>;

/**
 * Wrapper for calls that may need confirmation on production connections. The final version
 * catches `confirmationRequired`, asks through `ConfirmDestructiveDialog` and calls again with
 * the grant; this one only runs the call.
 */
export function useGuardedCall(): <T>(call: GuardedCall<T>) => Promise<T> {
  return (call) => call();
}
