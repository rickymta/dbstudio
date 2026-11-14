// Shared helpers for the debugger end-to-end tests (ipc + mock level, no components).
import { ipc } from '@/lib/ipc';
import { mock, resetMockEvents } from '@/lib/mock';
import { resetMockDebug } from '@/lib/mock/debug';
import {
  resetEditorWorld,
  PG_DEV,
  PG_PROD_RO,
  addProductionProfile,
} from '@/features/editor/testUtils';
import type {
  Breakpoint,
  ConfirmationGrant,
  DebugOutputEvent,
  DebugState,
  DebugStartRequest,
  PauseReason,
  Uuid,
} from '@/lib/types';

export { PG_DEV, PG_PROD_RO, addProductionProfile };

export function resetDebugWorld(): void {
  resetEditorWorld();
  resetMockDebug();
  resetMockEvents();
}

/** Open a session and a lease on `connectionId`. */
export async function openLease(connectionId: string = PG_DEV): Promise<Uuid> {
  const session = await ipc.sessionOpen(connectionId, 'pw');
  const lease = await ipc.leaseOpen(session.sessionId, {});
  return lease.leaseId;
}

export function bp(line: number, extra: Partial<Breakpoint> = {}): Breakpoint {
  return {
    id: `bp-${line}-${Math.random().toString(36).slice(2, 8)}`,
    line,
    enabled: true,
    suspend: true,
    ...extra,
  };
}

export async function startDebug(
  leaseId: Uuid,
  text: string,
  extra: Partial<DebugStartRequest> = {},
): Promise<DebugState> {
  return ipc.debugStart({ leaseId, text, ...extra });
}

/** Records `debug://state` and `debug://output` events in arrival order. */
export async function recordEvents(): Promise<{
  states: DebugState[];
  outputs: DebugOutputEvent[];
  seqs: number[];
  stop: () => void;
}> {
  const states: DebugState[] = [];
  const outputs: DebugOutputEvent[] = [];
  const seqs: number[] = [];
  const offState = await ipc.onEvent<DebugState>('debug://state', (e) => {
    states.push(e);
    seqs.push(e.seq);
  });
  const offOut = await ipc.onEvent<DebugOutputEvent>('debug://output', (e) => {
    outputs.push(e);
    seqs.push(e.seq);
  });
  return {
    states,
    outputs,
    seqs,
    stop: () => {
      offState();
      offOut();
    },
  };
}

export function pauseReason(state: DebugState): PauseReason['kind'] | null {
  return state.status.kind === 'paused' ? state.status.reason.kind : null;
}

/**
 * `ipc.debugStep` does not forward the confirmation grant to the browser mock, so tests that
 * answer a `confirmation` pause call the mock directly (the Tauri path forwards it).
 */
export function stepWithGrant(
  debugId: Uuid,
  grant: ConfirmationGrant,
  kind: 'resume' | 'stepOver' = 'stepOver',
): Promise<DebugState> {
  return mock.debugStep(debugId, { kind }, grant);
}
