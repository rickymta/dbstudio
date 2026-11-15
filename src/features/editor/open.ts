import i18n from '@/lib/i18n';
import type { ObjectPath, SessionInfo } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { editorTabId, useTabsStore, type EditorMode, type EditorTabPayload } from '@/stores/tabs';
import { newTabData, useEditorStore, type EditorTabData } from './editorStore';
import { registerEditorTab } from './editorRunner';

export interface OpenEditorOptions {
  /** Object the editor was opened from (database, schema or table node); sets the context. */
  path?: ObjectPath;
  /** Initial text of the new editor. */
  text?: string;
  /** Context to use instead of the one derived from `path`. */
  database?: string | null;
  schema?: string | null;
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

/** Titles of the editors that are open (tabs restored after a reload included). */
function openEditorTitles(): Set<string> {
  const titles = new Set<string>();
  for (const tab of useTabsStore.getState().tabs) {
    if (tab.kind === 'editor' && typeof tab.title === 'string') titles.add(tab.title);
  }
  for (const data of Object.values(useEditorStore.getState().tabs)) {
    if (data.title) titles.add(data.title);
  }
  return titles;
}

/**
 * "Query 3" in the current language. Numbers only grow within a run, and a number whose title an
 * open editor already carries (the counter restarts at a reload while restored tabs keep their
 * titles) is skipped, so titles stay distinct.
 */
export function nextEditorTitle(): string {
  const used = openEditorTitles();
  let title: string;
  do {
    queryNumber++;
    title = i18n.t('editor:tab.untitled', { n: queryNumber });
  } while (used.has(title));
  return title;
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
  const fromPath = contextOfPath(options.path);
  const database = options.database !== undefined ? options.database : fromPath.database;
  const schema = options.schema !== undefined ? options.schema : fromPath.schema;
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

/**
 * `body` with a statement terminator added after its last statement when it has none: the
 * trailing whitespace and comments (`--`, `//` for Mongo, block comments) are skipped, and the
 * `;` goes right after the last token, before those comments. Strings and quoted identifiers are
 * respected. Redis commands end at the newline, so they are never changed; neither is an empty
 * or comment-only body, nor one that ends inside an unterminated string or comment.
 */
export function terminateStatements(body: string, mode: EditorMode): string {
  if (mode === 'redis') return body;
  const lineComment = mode === 'mongo' ? '//' : '--';
  let last = -1; // index of the last significant character
  let i = 0;
  const n = body.length;
  while (i < n) {
    const c = body.charAt(i);
    if (body.startsWith(lineComment, i)) {
      const nl = body.indexOf('\n', i);
      i = nl < 0 ? n : nl + 1;
    } else if (body.startsWith('/*', i)) {
      const end = body.indexOf('*/', i + 2);
      if (end < 0) return body;
      i = end + 2;
    } else if (c === "'" || c === '"' || c === '`') {
      let j = i + 1;
      while (j < n && body.charAt(j) !== c)
        j += body.charAt(j) === '\\' && mode === 'mongo' ? 2 : 1;
      if (j >= n) return body;
      last = j;
      i = j + 1;
    } else {
      if (!/\s/.test(c)) last = i;
      i++;
    }
  }
  if (last < 0 || body.charAt(last) === ';') return body;
  return body.slice(0, last + 1) + ';' + body.slice(last + 1);
}

/**
 * Append `text` at the end of an open editor, one blank line after the existing text, and show
 * the tab (a missing statement terminator at the end of the existing text is added first, so the
 * new statement is not merged into the last one). Nothing is run. The new text is selected unless `select` is false (templates: the
 * cursor goes to the end so a stray Enter or Run does not take the whole statement).
 * Returns false when the editor is gone.
 */
export function appendToEditor(
  tabStateId: string,
  text: string,
  { select = true }: { select?: boolean } = {},
): boolean {
  const store = useEditorStore.getState();
  const data = store.tabs[tabStateId];
  if (!data) return false;
  const tab = useTabsStore
    .getState()
    .tabs.find((t) => t.kind === 'editor' && t.payload.tabStateId === tabStateId);
  const mode = tab?.kind === 'editor' ? tab.payload.mode : 'sql';
  const body = terminateStatements(data.text, mode);
  const gap =
    body.length === 0 ? '' : body.endsWith('\n\n') ? '' : body.endsWith('\n') ? '\n' : '\n\n';
  const from = body.length + gap.length;
  const to = from + text.length;
  store.patch(tabStateId, {
    text: body + gap + text,
    cursor: to,
    selection: select ? { from, to } : { from: to, to },
  });
  useTabsStore.getState().activate(editorTabId(tabStateId));
  return true;
}

/** Show `title` on the tab of an editor (file name after open / save). */
export function setEditorTitle(tabStateId: string, title: string): void {
  useEditorStore.getState().patch(tabStateId, { title });
  useTabsStore.setState((s) => ({
    tabs: s.tabs.map((t) => (t.id === editorTabId(tabStateId) ? { ...t, title } : t)),
  }));
}

/**
 * New editor on the connection of the active tab (else on the first open session that can run
 * statements), keeping the database / schema of the active editor. `false` when there is none.
 */
export function openEditorForActiveTab(): boolean {
  const { tabs, activeId } = useTabsStore.getState();
  const active = tabs.find((t) => t.id === activeId);
  const sessions = useSessionsStore.getState().sessionsByConnection;
  const connectionId = active?.payload?.connectionId;
  const own = connectionId ? sessions[connectionId] : undefined;
  const session =
    own && own.descriptor.capabilities.leases
      ? own
      : Object.values(sessions).find((s) => s.descriptor.capabilities.leases);
  if (!session) return false;
  const data =
    active?.kind === 'editor' && session === own
      ? useEditorStore.getState().tabs[active.payload.tabStateId]
      : undefined;
  openEditor(session, {
    path:
      session === own &&
      active &&
      (active.kind === 'table' || active.kind === 'ddl' || active.kind === 'key')
        ? active.payload.path
        : undefined,
    database: data ? (data.leaseState?.database ?? data.database) : undefined,
    schema: data ? (data.leaseState?.schema ?? data.schema) : undefined,
  });
  return true;
}

/** `/path/to/query.sql` -> `query.sql` */
export const fileBaseName = (path: string): string => path.split(/[\\/]/).pop() || path;
