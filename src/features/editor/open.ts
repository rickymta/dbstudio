import i18n from '@/lib/i18n';
import type { ObjectPath, SessionInfo } from '@/lib/types';
import { editorTabId, useTabsStore, type EditorMode, type EditorTabPayload } from '@/stores/tabs';
import { newTabData, useEditorStore, type EditorTabData } from './editorStore';
import { registerEditorTab } from './editorRunner';

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

/** Database and schema named by an object path (the nearest ones, walking up from the node). */
export function contextOfPath(path: ObjectPath | undefined): {
  database: string | null;
  schema: string | null;
} {
  let database: string | null = null;
  let schema: string | null = null;
  for (const seg of path?.segments ?? []) {
    if (seg.kind === 'database') database = seg.name;
    if (seg.kind === 'schema') schema = seg.name;
  }
  return { database, schema };
}

let queryNumber = 0;

/** "Query 3" in the current language; numbers only grow so titles stay distinct. */
export function nextEditorTitle(): string {
  queryNumber++;
  return i18n.t('editor:tab.untitled', { n: queryNumber });
}

/** Register a new editor: its state, close guard and tab. Returns the id of the editor state. */
export function addEditorTab(
  payload: EditorTabPayload,
  init: Partial<EditorTabData>,
  activate = true,
): string {
  const id = payload.tabStateId;
  useEditorStore.getState().ensure(id, newTabData(id, init));
  registerEditorTab(id);
  const store = useTabsStore.getState();
  const previous = store.activeId;
  store.openTab({
    id: editorTabId(id),
    kind: 'editor',
    title: init.title ?? { key: 'tab.title', ns: 'editor' },
    closable: true,
    payload,
  });
  if (!activate) useTabsStore.getState().activate(previous);
  return id;
}

/** Open a new editor tab for `session`; returns the id of the editor state. */
export function openEditor(session: SessionInfo, options: OpenEditorOptions = {}): string {
  const tabStateId = crypto.randomUUID();
  const text = options.text ?? '';
  const { database, schema } = contextOfPath(options.path);
  return addEditorTab(
    {
      tabStateId,
      connectionId: session.connectionId,
      sessionId: session.sessionId,
      mode: editorModeOf(session),
    },
    {
      title: nextEditorTitle(),
      text,
      cursor: text.length,
      selection: { from: text.length, to: text.length },
      database,
      schema,
    },
  );
}
