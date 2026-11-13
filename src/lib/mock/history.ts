// Mock query history, saved queries, persisted editor tabs and .sql files (in memory; the
// editor tabs also survive a browser reload through localStorage so tab restore can be tried).

import type {
  EditorTabState,
  HistoryEntry,
  HistoryQuery,
  SavedFolder,
  SavedQueriesSnapshot,
  SavedQuery,
  SqlFile,
  Uuid,
} from '../types';
import { clone } from './util';

const TABS_KEY = 'dbstudio.mock.editorTabs';

function loadTabs(): EditorTabState[] {
  try {
    const raw = localStorage.getItem(TABS_KEY);
    return raw ? (JSON.parse(raw) as EditorTabState[]) : [];
  } catch {
    return [];
  }
}

let history: HistoryEntry[] = [];
let nextHistoryId = 1;
let folders: SavedFolder[] = [];
let queries: SavedQuery[] = [];
let tabs: EditorTabState[] = loadTabs();
/** "Disk": files saved through `sql_file_save`, and the one `sql_file_open` returns next. */
let files = new Map<string, string>();
let openNext: SqlFile | null = null;
let savedCounter = 0;

const HISTORY_TEXT_LIMIT = 100_000;

export interface MockHistoryInput {
  connectionId: Uuid;
  database: string | null;
  text: string;
  kind: string;
}

/** Add a `running` entry (what `query_execute` / `lease_execute` do when history is on). */
export function mockRecordHistory(input: MockHistoryInput): number {
  const id = nextHistoryId++;
  history.push({
    id,
    connectionId: input.connectionId,
    database: input.database,
    kind: input.kind,
    text: input.text.slice(0, HISTORY_TEXT_LIMIT),
    textTruncated: input.text.length > HISTORY_TEXT_LIMIT,
    startedAt: new Date().toISOString(),
    elapsedMs: null,
    status: 'running',
    statements: null,
    rows: null,
    errorMessage: null,
  });
  return id;
}

export function mockFinishHistory(
  id: number,
  status: 'ok' | 'error' | 'cancelled',
  elapsedMs: number,
  statements: number,
  rows: number,
  errorMessage: string | null,
): void {
  const entry = history.find((h) => h.id === id);
  if (!entry) return;
  entry.status = status;
  entry.elapsedMs = elapsedMs;
  entry.statements = statements;
  entry.rows = rows;
  entry.errorMessage = errorMessage;
}

/** Test/dev helper: the file `sql_file_open` returns next (`null` = the dialog is cancelled). */
export function mockSetSqlFileToOpen(file: SqlFile | null): void {
  openNext = file;
}

/** Test/dev helper: contents of a file saved through `sql_file_save`. */
export function mockReadSqlFile(path: string): string | undefined {
  return files.get(path);
}

export const historyMock = {
  async historyList(query: HistoryQuery): Promise<HistoryEntry[]> {
    return clone(
      history
        .filter((h) => !query.connectionId || h.connectionId === query.connectionId)
        .filter((h) => !query.status || h.status === query.status)
        .filter((h) => !query.search || h.text.toLowerCase().includes(query.search.toLowerCase()))
        .filter((h) => query.beforeId == null || h.id < query.beforeId)
        .sort((a, b) => b.id - a.id)
        .slice(0, Math.min(Math.max(query.limit, 1), 500)),
    );
  },
  async historyDelete(ids: number[]): Promise<void> {
    history = history.filter((h) => !ids.includes(h.id));
  },
  async historyClear(connectionId: Uuid | null): Promise<number> {
    const before = history.length;
    history = connectionId ? history.filter((h) => h.connectionId !== connectionId) : [];
    return before - history.length;
  },
  async savedQueriesList(): Promise<SavedQueriesSnapshot> {
    return clone({ folders, queries });
  },
  async savedQuerySave(query: SavedQuery): Promise<SavedQuery> {
    queries = [...queries.filter((q) => q.id !== query.id), clone(query)];
    return clone(query);
  },
  async savedQueryDelete(id: Uuid): Promise<void> {
    queries = queries.filter((q) => q.id !== id);
  },
  async savedFolderSave(folder: SavedFolder): Promise<SavedFolder> {
    folders = [...folders.filter((f) => f.id !== folder.id), clone(folder)];
    return clone(folder);
  },
  async savedFolderDelete(id: Uuid): Promise<void> {
    folders = folders.filter((f) => f.id !== id);
    queries = queries.map((q) => (q.folderId === id ? { ...q, folderId: null } : q));
  },
  async editorTabsGet(): Promise<EditorTabState[]> {
    return clone(tabs).sort((a, b) => a.position - b.position);
  },
  async editorTabsSet(next: EditorTabState[]): Promise<void> {
    tabs = clone(next);
    try {
      localStorage.setItem(TABS_KEY, JSON.stringify(tabs));
    } catch {
      // storage unavailable: tabs only live in memory
    }
  },
  async sqlFileOpen(): Promise<SqlFile | null> {
    const file = openNext;
    openNext = null;
    if (file) files.set(file.path, file.text);
    return file ? clone(file) : null;
  },
  async sqlFileSave(path: string | null, text: string): Promise<string | null> {
    const target = path ?? `/mock/query-${++savedCounter}.sql`;
    files.set(target, text);
    return target;
  },
};

export function resetMockHistory(): void {
  history = [];
  nextHistoryId = 1;
  folders = [];
  queries = [];
  tabs = [];
  files = new Map();
  openNext = null;
  savedCounter = 0;
  try {
    localStorage.removeItem(TABS_KEY);
  } catch {
    // ignore
  }
}
