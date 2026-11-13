import { mock } from '../mock';
import type {
  ConfirmationGrant,
  ExportPreset,
  ExportSpec,
  ImportPreview,
  ImportPreviewRequest,
  ImportSpec,
  JobId,
  Uuid,
} from '../types';
import { call } from './core';

/** Export / import (jobs) and export presets. */
export const exportApi = {
  exportStart: (spec: ExportSpec): Promise<JobId> =>
    call('export_start', () => mock.exportStart(spec), { spec }),
  exportPresetsList: (connectionId?: Uuid | null): Promise<ExportPreset[]> =>
    call('export_presets_list', () => mock.exportPresetsList(connectionId ?? null), {
      connectionId: connectionId ?? null,
    }),
  exportPresetSave: (preset: ExportPreset): Promise<ExportPreset> =>
    call('export_preset_save', () => mock.exportPresetSave(preset), { preset }),
  exportPresetDelete: (id: Uuid): Promise<void> =>
    call('export_preset_delete', () => mock.exportPresetDelete(id), { id }),
  importPreview: (request: ImportPreviewRequest): Promise<ImportPreview> =>
    call('import_preview', () => mock.importPreview(request), { request }),
  /** `confirm` answers a `confirmationRequired` error (script import into production). */
  importStart: (spec: ImportSpec, confirm?: ConfirmationGrant | null): Promise<JobId> =>
    call('import_start', () => mock.importStart(spec, confirm ?? null), {
      spec,
      confirm: confirm ?? null,
    }),
};
