import { create } from 'zustand';
import type { ObjectPath, SessionInfo } from '@/lib/types';

/** What the export wizard starts from. */
export interface ExportOrigin {
  session: SessionInfo;
  /** Database, schema, table or collection node; `undefined` exports from a query. */
  path?: ObjectPath;
  /** Query text to export (with `path` undefined). */
  query?: string;
}

/** What the import wizard starts from. */
export interface ImportOrigin {
  session: SessionInfo;
  /** Table to import into; omitted = new table or SQL script. */
  target?: ObjectPath;
  /** Schema / database node new tables are created in. */
  parent?: ObjectPath;
}

interface WizardState {
  exportOrigin: ExportOrigin | null;
  importOrigin: ImportOrigin | null;
  setExport: (origin: ExportOrigin | null) => void;
  setImport: (origin: ImportOrigin | null) => void;
}

export const useWizardStore = create<WizardState>((set) => ({
  exportOrigin: null,
  importOrigin: null,
  setExport: (exportOrigin) => set({ exportOrigin }),
  setImport: (importOrigin) => set({ importOrigin }),
}));

/** Open the export wizard for `origin` (rendered by the host in the status bar). */
export function openExport(origin: ExportOrigin): void {
  useWizardStore.getState().setExport(origin);
}

/** Open the import wizard for `origin`. */
export function openImport(origin: ImportOrigin): void {
  useWizardStore.getState().setImport(origin);
}
