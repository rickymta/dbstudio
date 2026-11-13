import type { TFunction } from 'i18next';
import { toIpcError } from '@/lib/ipc';
import type { IpcError } from '@/lib/types';

/** Human readable "<kind label>: <message>" for a connection related error. */
export function describeError(t: TFunction, error: unknown): string {
  const e: IpcError = toIpcError(error);
  const label = t(`connections:errorKind.${e.kind}`, { defaultValue: '' });
  return label ? `${label}: ${e.message}` : e.message;
}
