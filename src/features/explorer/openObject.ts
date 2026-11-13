import type { ObjectNode, SessionInfo } from '@/lib/types';
import { ddlTabId, tableTabId, useTabsStore } from '@/stores/tabs';
import { activationTab } from './treeModel';

/**
 * Open the data or DDL tab of an object like activating it in the tree does.
 * Returns false when the kind (or the driver's capabilities) has no tab.
 */
export function openObjectTab(node: ObjectNode, session: SessionInfo): boolean {
  const tab = activationTab(node.kind, session.descriptor.capabilities);
  if (!tab) return false;
  const payload = {
    sessionId: session.sessionId,
    connectionId: session.connectionId,
    path: node.path,
  };
  const { openTab } = useTabsStore.getState();
  if (tab === 'table') {
    openTab({
      id: tableTabId(session.sessionId, node.path),
      kind: 'table',
      title: node.name,
      closable: true,
      payload,
    });
  } else {
    openTab({
      id: ddlTabId(session.sessionId, node.path),
      kind: 'ddl',
      title: node.name,
      closable: true,
      payload,
    });
  }
  return true;
}
