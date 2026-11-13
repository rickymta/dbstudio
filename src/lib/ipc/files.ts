import { mock } from '../mock';
import type { FileFilter } from '../types';
import { call } from './core';

/** Native file dialogs; every returned path is granted to the backend for a limited time. */
export const filesApi = {
  /** Resolves to the chosen path, `null` when cancelled. */
  filePickSave: (suggestedName: string, filters: FileFilter[]): Promise<string | null> =>
    call('file_pick_save', () => mock.filePickSave(suggestedName, filters), {
      suggestedName,
      filters,
    }),
  filePickOpen: (filters: FileFilter[], directory: boolean): Promise<string | null> =>
    call('file_pick_open', () => mock.filePickOpen(filters, directory), { filters, directory }),
  /** Save bytes (base64) through a save dialog (ERD PNG/SVG/PDF...). */
  fileSaveBytes: (
    suggestedName: string,
    dataBase64: string,
    filters: FileFilter[],
  ): Promise<string | null> =>
    call('file_save_bytes', () => mock.fileSaveBytes(suggestedName, dataBase64, filters), {
      suggestedName,
      dataBase64,
      filters,
    }),
};
