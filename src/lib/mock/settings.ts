import { DEFAULT_SETTINGS, type AppSettings, type SupportRequest } from '../types';
import { clone, ipcError, sleep } from './util';

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
  // Same ranges as the backend, for the fields the settings dialog edits.
  const inRange = (n: number, min: number, max: number) =>
    Number.isInteger(n) && n >= min && n <= max;
  const checks: [string, boolean][] = [
    ['gridMaxRows', inRange(next.gridMaxRows, 1_000, 5_000_000)],
    ['gridMaxCells', inRange(next.gridMaxCells, 100_000, 200_000_000)],
    ['editorMaxRows', inRange(next.editorMaxRows, 100, 1_000_000)],
    ['queryTimeoutMs', inRange(next.queryTimeoutMs, 0, 86_400_000)],
    ['historyMaxPerConnection', inRange(next.historyMaxPerConnection, 100, 100_000)],
    ['keepaliveSecs', next.keepaliveSecs === 0 || inRange(next.keepaliveSecs, 30, 3_600)],
    ['planHistoryMaxPerConnection', inRange(next.planHistoryMaxPerConnection, 10, 5_000)],
  ];
  const bad = checks.find(([, ok]) => !ok);
  if (bad) throw ipcError('invalid', `${bad[0]} is out of range`);
  settings = clone(next);
  return clone(settings);
}

export const mockLogDir = '/mock/logs/dbstudio';

export async function saveCellValue(suggestedName: string, dataBase64: string): Promise<string> {
  await sleep(5, 20);
  void dataBase64; // the mock writes nothing
  return `/mock/downloads/${suggestedName}`;
}

/** Requests "sent" through the mock, newest last (nothing leaves the browser). */
export const mockSupportOutbox: SupportRequest[] = [];

export async function supportSend(request: SupportRequest): Promise<void> {
  await sleep(100, 300);
  if (!request.email.includes('@')) throw ipcError('invalid', 'the email address is not valid');
  mockSupportOutbox.push(clone(request));
}
