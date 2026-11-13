import type { ObjectPath, SessionInfo } from '@/lib/types';

/** What the export wizard starts from. */
export interface ExportOrigin {
  session: SessionInfo;
  /** Database, schema, table or collection node; `undefined` exports from a query. */
  path?: ObjectPath;
}

/** Open the export wizard for `origin`. Does nothing until the wizard exists. */
export function openExport(origin: ExportOrigin): void {
  void origin;
}
