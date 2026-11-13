import type { ObjectPath, SessionInfo } from '@/lib/types';
import { erdTabId, useTabsStore } from '@/stores/tabs';

/** Open (or focus) the ER diagram of `scope` (a database or schema path). */
export function openErd(session: SessionInfo, scope: ObjectPath, layoutId: string | null = null) {
  useTabsStore.getState().openTab({
    id: erdTabId(session.sessionId, scope),
    kind: 'erd',
    title: { key: 'tab.title', ns: 'erd' },
    closable: true,
    payload: {
      sessionId: session.sessionId,
      connectionId: session.connectionId,
      scope,
      layoutId,
    },
  });
}
