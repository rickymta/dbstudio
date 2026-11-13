import type { JobEvent, LeaseStateEvent } from '../types';
import { onEvent } from './core';

/** Names of the events the backend emits. */
export const EVENTS = {
  jobProgress: 'job://progress',
  leaseState: 'lease://state',
  backupChanged: 'backup://changed',
} as const;

/** Typed payloads of `EVENTS`. */
export interface EventPayloads {
  'job://progress': JobEvent;
  'lease://state': LeaseStateEvent;
  'backup://changed': null;
}

export const eventsApi = {
  /**
   * Subscribe to a backend event (Tauri `listen`, the bridge `/events` NDJSON stream, or the
   * mock emitter). Resolves to the function that unsubscribes.
   */
  onEvent: <T = unknown>(name: string, handler: (payload: T) => void): Promise<() => void> =>
    onEvent<T>(name, handler),
};
