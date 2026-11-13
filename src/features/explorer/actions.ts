import { openBackup } from '@/features/backup/open';
import { openEditor } from '@/features/editor/open';
import { openErd } from '@/features/erd/open';
import { openExport } from '@/features/export/open';
import type { DriverDescriptor, ObjectKind, ObjectNode, SessionInfo } from '@/lib/types';

export type ExplorerActionId = 'openEditor' | 'showErd' | 'exportData' | 'backup';

export interface ExplorerAction {
  id: ExplorerActionId;
  /** Translation key (namespace `explorer`). */
  labelKey: `explorer:action.${ExplorerActionId}`;
  run: (session: SessionInfo) => void;
}

const SCOPE_KINDS: ReadonlySet<ObjectKind> = new Set(['database', 'schema']);
const EXPORT_KINDS: ReadonlySet<ObjectKind> = new Set([
  'database',
  'schema',
  'table',
  'view',
  'collection',
]);

/**
 * Context-menu actions that open the editor, diagram, export and backup features for `node`,
 * in display order. The explorer menu shows them; each `run` hands off to its feature's
 * `open.ts`.
 */
export function explorerActions(node: ObjectNode, descriptor: DriverDescriptor): ExplorerAction[] {
  const caps = descriptor.capabilities;
  const actions: ExplorerAction[] = [];
  if (
    caps.leases &&
    (SCOPE_KINDS.has(node.kind) || node.kind === 'table' || node.kind === 'collection')
  ) {
    actions.push({
      id: 'openEditor',
      labelKey: 'explorer:action.openEditor',
      run: (session) => openEditor(session, { path: node.path }),
    });
  }
  if (caps.schemaModel && SCOPE_KINDS.has(node.kind)) {
    actions.push({
      id: 'showErd',
      labelKey: 'explorer:action.showErd',
      run: (session) => openErd(session, node.path),
    });
  }
  if (caps.tableStream && EXPORT_KINDS.has(node.kind)) {
    actions.push({
      id: 'exportData',
      labelKey: 'explorer:action.exportData',
      run: (session) => openExport({ session, path: node.path }),
    });
  }
  if (caps.logicalBackup && node.kind === 'database') {
    actions.push({
      id: 'backup',
      labelKey: 'explorer:action.backup',
      run: () => openBackup(),
    });
  }
  return actions;
}
