import { DEFAULT_SETTINGS, type AppSettings } from '../types';
import { clone, sleep } from './util';

let settings: AppSettings = { ...DEFAULT_SETTINGS };

export function resetMockSettings(): void {
  settings = { ...DEFAULT_SETTINGS };
}

export async function settingsGet(): Promise<AppSettings> {
  await sleep(5, 20);
  return clone(settings);
}

export async function settingsSet(next: AppSettings): Promise<AppSettings> {
  await sleep(5, 20);
  settings = clone(next);
  return clone(settings);
}

export const mockLogDir = '/mock/logs/dbstudio';

export async function saveCellValue(suggestedName: string, dataBase64: string): Promise<string> {
  await sleep(5, 20);
  void dataBase64; // the mock writes nothing
  return `/mock/downloads/${suggestedName}`;
}
