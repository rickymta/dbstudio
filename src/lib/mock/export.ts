// Mock export / import: presets live in memory, jobs are unsupported, file dialogs cancel.

import type {
  ConfirmationGrant,
  ExportPreset,
  ExportSpec,
  FileFilter,
  ImportPreview,
  ImportPreviewRequest,
  ImportSpec,
  JobId,
  Uuid,
} from '../types';
import { clone, unsupported } from './util';

let presets: ExportPreset[] = [];

export const exportMock = {
  async filePickSave(suggestedName: string, filters: FileFilter[]): Promise<string | null> {
    void [suggestedName, filters];
    return null;
  },
  async filePickOpen(filters: FileFilter[], directory: boolean): Promise<string | null> {
    void [filters, directory];
    return null;
  },
  async fileSaveBytes(
    suggestedName: string,
    dataBase64: string,
    filters: FileFilter[],
  ): Promise<string | null> {
    void [dataBase64, filters];
    return `/mock/downloads/${suggestedName}`;
  },
  async exportStart(spec: ExportSpec): Promise<JobId> {
    void spec;
    throw unsupported('export');
  },
  async exportPresetsList(connectionId: Uuid | null): Promise<ExportPreset[]> {
    return clone(presets.filter((p) => !connectionId || p.connectionId === connectionId));
  },
  async exportPresetSave(preset: ExportPreset): Promise<ExportPreset> {
    presets = [...presets.filter((p) => p.id !== preset.id), clone(preset)];
    return clone(preset);
  },
  async exportPresetDelete(id: Uuid): Promise<void> {
    presets = presets.filter((p) => p.id !== id);
  },
  async importPreview(request: ImportPreviewRequest): Promise<ImportPreview> {
    void request;
    throw unsupported('import');
  },
  async importStart(spec: ImportSpec, confirm: ConfirmationGrant | null): Promise<JobId> {
    void [spec, confirm];
    throw unsupported('import');
  },
};

export function resetMockExport(): void {
  presets = [];
}
