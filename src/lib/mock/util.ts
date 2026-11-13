import type { IpcError, Uuid } from '../types';

/** Resolve after a random 50-200 ms, emulating IPC and database latency. */
export function sleep(min = 50, max = 200): Promise<void> {
  const ms = min + Math.random() * (max - min);
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export function ipcError(
  kind: IpcError['kind'],
  message: string,
  extra?: Partial<IpcError>,
): IpcError {
  return { kind, message, ...extra };
}

export function newId(): Uuid {
  return crypto.randomUUID();
}

export function clone<T>(value: T): T {
  return structuredClone(value);
}
