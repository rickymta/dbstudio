import { ipc, ipcTransport, toIpcError } from '@/lib/ipc';
import { applyKeyChanges, previewKeyChanges } from '@/lib/mock/keyvalue';
import type { ApplyResult, ChangeSet, IpcError, Uuid } from '@/lib/types';

/**
 * Commands `changes_apply` would run for a key change set. In the browser mock the generic
 * `changes` mock does not know the `keys` model, so the key-value mock answers instead.
 */
export async function previewKeyChangeSet(sessionId: Uuid, set: ChangeSet): Promise<string[]> {
  if (set.model === 'keys' && ipcTransport() === 'mock') return previewKeyChanges(set);
  return ipc.changesPreview(sessionId, set);
}

/** Apply a key change set (see `previewKeyChangeSet` for the mock fallback). */
export async function applyKeyChangeSet(sessionId: Uuid, set: ChangeSet): Promise<ApplyResult> {
  try {
    return await ipc.changesApply(sessionId, set);
  } catch (e) {
    const err = toIpcError(e);
    if (
      set.model === 'keys' &&
      ipcTransport() === 'mock' &&
      err.kind === 'unsupported' &&
      err.capability === 'data_editing'
    ) {
      return applyKeyChanges(set);
    }
    throw e;
  }
}

/** Failure of change #`index`: Redis does not roll back, `applied` changes already took effect. */
export function changeFailure(
  e: unknown,
): { error: IpcError; index: number; applied: number } | null {
  const error = toIpcError(e);
  if (error.kind !== 'changeFailed') return null;
  return { error, index: error.index ?? 0, applied: error.applied ?? 0 };
}
