import { invoke } from '@tauri-apps/api/core';
import { mockAppInfo, mockDrivers } from './mock';
import type { AppInfo, DriverDescriptor } from './types';

/** True when running inside the Tauri webview (as opposed to a plain browser during dev). */
export function isTauri(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

/**
 * Call a backend command, or the mock when running outside Tauri.
 * All backend access must go through this module.
 */
async function call<T>(
  command: string,
  mock: () => T | Promise<T>,
  args?: Record<string, unknown>,
): Promise<T> {
  if (!isTauri()) return mock();
  return invoke<T>(command, args);
}

export const ipc = {
  appInfo: (): Promise<AppInfo> => call('app_info', () => mockAppInfo),
  listDrivers: (): Promise<DriverDescriptor[]> => call('list_drivers', () => mockDrivers),
};
