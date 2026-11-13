import { ipc, toIpcError } from '@/lib/ipc';
import type { ApplyResult, ChangeSet, IpcError, Uuid } from '@/lib/types';

/** Commands `changes_apply` would run for a key change set. */
export function previewKeyChangeSet(sessionId: Uuid, set: ChangeSet): Promise<string[]> {
  return ipc.changesPreview(sessionId, set);
}

/** Apply a key change set. */
export function applyKeyChangeSet(sessionId: Uuid, set: ChangeSet): Promise<ApplyResult> {
  return ipc.changesApply(sessionId, set);
}

/** Failure of change #`index`: Redis does not roll back, `applied` changes already took effect. */
export function changeFailure(
  e: unknown,
): { error: IpcError; index: number; applied: number } | null {
  const error = toIpcError(e);
  if (error.kind !== 'changeFailed') return null;
  return { error, index: error.index ?? 0, applied: error.applied ?? 0 };
}
