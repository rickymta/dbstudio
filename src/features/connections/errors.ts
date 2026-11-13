import type { TFunction } from 'i18next';
import { toIpcError } from '@/lib/ipc';
import type { IpcError } from '@/lib/types';

/**
 * English kind prefixes older backends put at the start of `message` ("connection error: ...").
 * The UI adds its own translated label, so they are dropped to avoid saying it twice.
 */
const KIND_PREFIX =
  /^(?:(?:connection|authentication|query|tls|storage|io|i\/o|other)(?: error| failed| failure)?|(?:unsupported|timeout|timed out|not found|cancelled)(?: error)?):\s+/i;

export function stripKindPrefix(message: string): string {
  const stripped = message.replace(KIND_PREFIX, '');
  return stripped || message;
}

/** Human readable "<translated kind label>: <message>" for an IPC error. */
export function describeError(t: TFunction, error: unknown): string {
  const e: IpcError = toIpcError(error);
  const message = stripKindPrefix(e.message);
  const label = t(`connections:errorKind.${e.kind}`, { defaultValue: '' });
  return label ? `${label}: ${message}` : message;
}
