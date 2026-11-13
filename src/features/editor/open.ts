import type { ObjectPath, SessionInfo } from '@/lib/types';
import { editorTabId, useTabsStore, type EditorMode } from '@/stores/tabs';

export interface OpenEditorOptions {
  /** Object the editor was opened from (database, schema or table node); sets the context. */
  path?: ObjectPath;
  /** Initial text of the new editor. */
  text?: string;
}

/** Editor language of a session's script dialect. */
export function editorModeOf(session: SessionInfo): EditorMode {
  switch (session.dialect.name) {
    case 'mongoShell':
      return 'mongo';
    case 'redisCli':
      return 'redis';
    default:
      return 'sql';
  }
}

/** Open a new editor tab for `session`; returns the id of the editor state. */
export function openEditor(session: SessionInfo, options: OpenEditorOptions = {}): string {
  void options;
  const tabStateId = crypto.randomUUID();
  useTabsStore.getState().openTab({
    id: editorTabId(tabStateId),
    kind: 'editor',
    title: { key: 'tab.title', ns: 'editor' },
    closable: true,
    payload: {
      tabStateId,
      connectionId: session.connectionId,
      sessionId: session.sessionId,
      mode: editorModeOf(session),
    },
  });
  return tabStateId;
}
