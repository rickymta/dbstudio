import { open, save } from '@tauri-apps/plugin-dialog';
import { isTauri } from './ipc';

export interface FileFilter {
  name: string;
  extensions: string[];
}

/** Native "open file" dialog. Resolves to the chosen path, or `null` when cancelled (always `null` outside Tauri). */
export async function pickFile(options?: {
  title?: string;
  filters?: FileFilter[];
}): Promise<string | null> {
  if (!isTauri()) return null;
  const result = await open({ multiple: false, directory: false, ...options });
  return typeof result === 'string' ? result : null;
}

/** Native "save file" dialog. Resolves to the chosen path, or `null` when cancelled (always `null` outside Tauri). */
export async function pickSavePath(options?: {
  title?: string;
  defaultPath?: string;
  filters?: FileFilter[];
}): Promise<string | null> {
  if (!isTauri()) return null;
  return save(options);
}
