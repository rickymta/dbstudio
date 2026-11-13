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
import { mockRunJob } from './jobs';
import { clone, ipcError } from './util';

/** Connection id of the mock production profile: importing into it needs a confirmation. */
export const MOCK_PRODUCTION_CONNECTION = '00000000-0000-4000-8000-0000000000b3';

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
    if (spec.destination.path === '') throw ipcError('permission', 'path not granted: (empty)');
    const name = spec.destination.path.split('/').pop() ?? spec.destination.path;
    return mockRunJob({
      kind: 'export',
      title: `Export → ${name}`,
      connectionId: spec.connectionId,
      rowsTotal: 2550,
      result: {
        path: spec.destination.path,
        files: [spec.destination.path],
        sizeBytes: 183_000,
        rows: 2550,
        objects: 2,
        warnings: [],
      },
    });
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
    if (request.format.kind === 'sql') {
      const stmt = (index: number, verb: string, destructive: boolean) => ({
        index,
        range: { start: index * 10, end: index * 10 + 9 },
        class: { kind: 'other' },
        verb,
        destructive: destructive ? 'drop' : null,
        implicitCommit: false,
        targets: ['t'],
      });
      return {
        columns: [],
        rows: [],
        sheets: [],
        detectedDelimiter: null,
        suggestedKinds: [],
        estimatedRows: null,
        statementsPreview: [stmt(0, 'CREATE TABLE', false), stmt(1, 'DROP TABLE', true)],
      } as unknown as ImportPreview;
    }
    return {
      columns: ['id', 'name', 'price', 'added'],
      rows: [
        [
          { t: 'text', v: '1' },
          { t: 'text', v: 'Widget' },
          { t: 'text', v: '9.50' },
          { t: 'text', v: '2024-01-02' },
        ],
        [
          { t: 'text', v: '2' },
          { t: 'text', v: 'Gadget' },
          { t: 'text', v: '12.00' },
          { t: 'text', v: '2024-02-03' },
        ],
      ],
      sheets: request.format.kind === 'xlsx' ? ['Sheet1', 'Other'] : [],
      detectedDelimiter: request.format.kind === 'csv' ? ';' : null,
      suggestedKinds: ['int', 'text', 'float', 'date'],
      estimatedRows: 2,
      statementsPreview: [],
    };
  },
  async importStart(spec: ImportSpec, confirm: ConfirmationGrant | null): Promise<JobId> {
    if (
      spec.connectionId === MOCK_PRODUCTION_CONNECTION &&
      !(confirm && confirm.typed === 'prod_db')
    ) {
      throw ipcError('confirmationRequired', 'confirmation required', {
        confirmation: {
          token: 'mock-token',
          confirmText: 'prod_db',
          statements: [],
          expiresInMs: 120_000,
        },
      });
    }
    return mockRunJob({
      kind: 'import',
      title: `Import ${spec.file.split('/').pop() ?? spec.file}`,
      connectionId: spec.connectionId,
      rowsTotal: 2,
      result: { rowsRead: 2, inserted: 2, updated: 0, skipped: 0, errors: [] },
    });
  },
};

export function resetMockExport(): void {
  presets = [];
}
