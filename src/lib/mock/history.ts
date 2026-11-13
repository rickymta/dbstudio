// Mock query history, saved queries and persisted editor tabs (in memory).

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

let history: HistoryEntry[] = [];
let folders: SavedFolder[] = [];
let queries: SavedQuery[] = [];
let tabs: EditorTabState[] = [];

export const historyMock = {
  async historyList(query: HistoryQuery): Promise<HistoryEntry[]> {
    return clone(
      history
        .filter((h) => !query.connectionId || h.connectionId === query.connectionId)
        .filter((h) => !query.status || h.status === query.status)
        .filter((h) => !query.search || h.text.toLowerCase().includes(query.search.toLowerCase()))
        .filter((h) => query.beforeId == null || h.id < query.beforeId)
        .sort((a, b) => b.id - a.id)
        .slice(0, query.limit),
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
  },
  async editorTabsGet(): Promise<EditorTabState[]> {
    return clone(tabs);
  },
  async editorTabsSet(next: EditorTabState[]): Promise<void> {
    tabs = clone(next);
  },
  async sqlFileOpen(): Promise<SqlFile | null> {
    return null;
  },
  async sqlFileSave(path: string | null, text: string): Promise<string | null> {
    void text;
    return path;
  },
};

export function resetMockHistory(): void {
  history = [];
  folders = [];
  queries = [];
  tabs = [];
}
