import { mock } from '../mock';
import type {
  EditorTabState,
  ExecuteRequest,
  ExecuteResult,
  HistoryEntry,
  HistoryQuery,
  LeaseCloseReport,
  LeaseContext,
  LeaseInfo,
  LeaseOptions,
  LeaseReopened,
  LeaseState,
  ObjectPath,
  QueryEvent,
  SavedFolder,
  SavedQueriesSnapshot,
  SavedQuery,
  ScriptAnalysis,
  ScriptGenerated,
  ScriptTemplate,
  SqlFile,
  TxOptions,
  Uuid,
  Vocabulary,
} from '../types';
import { call, streamCommand } from './core';

/** A script result is complete after one `statementEnd` per statement that ran. */
const statementEnds = (r: ExecuteResult): number => r.summary.statements;

/** SQL editor / console: script analysis, leases, history, saved queries, editor tabs, files. */
export const editorApi = {
  scriptAnalyze: (sessionId: Uuid, text: string): Promise<ScriptAnalysis> =>
    call('script_analyze', () => mock.scriptAnalyze(sessionId, text), { sessionId, text }),
  scriptVocabulary: (sessionId: Uuid): Promise<Vocabulary> =>
    call('script_vocabulary', () => mock.scriptVocabulary(sessionId), { sessionId }),

  /** Run a script on a pooled connection (no state kept); streams like `tableFetch`. */
  queryExecute: (
    sessionId: Uuid,
    request: ExecuteRequest,
    onEvent: (e: QueryEvent) => void,
  ): Promise<ExecuteResult> =>
    streamCommand<ExecuteResult>({
      command: 'query_execute',
      args: { sessionId, request },
      onEvent,
      queryId: request.queryId,
      expectedEnds: statementEnds,
      mock: (handler) => mock.queryExecute(sessionId, request, handler),
    }),

  leaseOpen: (sessionId: Uuid, options: LeaseOptions): Promise<LeaseInfo> =>
    call('lease_open', () => mock.leaseOpen(sessionId, options), { sessionId, options }),
  leaseClose: (leaseId: Uuid): Promise<LeaseCloseReport> =>
    call('lease_close', () => mock.leaseClose(leaseId), { leaseId }),
  leaseExecute: (
    leaseId: Uuid,
    request: ExecuteRequest,
    onEvent: (e: QueryEvent) => void,
  ): Promise<ExecuteResult> =>
    streamCommand<ExecuteResult>({
      command: 'lease_execute',
      args: { leaseId, request },
      onEvent,
      queryId: request.queryId,
      expectedEnds: statementEnds,
      mock: (handler) => mock.leaseExecute(leaseId, request, handler),
    }),
  /** Continue the pending (truncated) result set of `queryId`. */
  leaseFetchMore: (
    leaseId: Uuid,
    queryId: Uuid,
    maxRows: number,
    batchSize: number | null,
    onEvent: (e: QueryEvent) => void,
  ): Promise<ExecuteResult> =>
    streamCommand<ExecuteResult>({
      command: 'lease_fetch_more',
      args: { leaseId, queryId, maxRows, batchSize },
      onEvent,
      queryId,
      expectedEnds: statementEnds,
      mock: (handler) => mock.leaseFetchMore(leaseId, queryId, maxRows, batchSize, handler),
    }),
  leaseDiscardResult: (leaseId: Uuid): Promise<LeaseState> =>
    call('lease_discard_result', () => mock.leaseDiscardResult(leaseId), { leaseId }),
  leaseCancel: (leaseId: Uuid, queryId: Uuid): Promise<void> =>
    call('lease_cancel', () => mock.leaseCancel(leaseId, queryId), { leaseId, queryId }),
  leaseBegin: (leaseId: Uuid, options: TxOptions): Promise<LeaseState> =>
    call('lease_begin', () => mock.leaseBegin(leaseId, options), { leaseId, options }),
  leaseCommit: (leaseId: Uuid): Promise<LeaseState> =>
    call('lease_commit', () => mock.leaseCommit(leaseId), { leaseId }),
  leaseRollback: (leaseId: Uuid): Promise<LeaseState> =>
    call('lease_rollback', () => mock.leaseRollback(leaseId), { leaseId }),
  leaseSetAutoCommit: (leaseId: Uuid, autoCommit: boolean): Promise<LeaseState> =>
    call('lease_set_auto_commit', () => mock.leaseSetAutoCommit(leaseId, autoCommit), {
      leaseId,
      autoCommit,
    }),
  leaseSetContext: (leaseId: Uuid, context: LeaseContext): Promise<LeaseState> =>
    call('lease_set_context', () => mock.leaseSetContext(leaseId, context), { leaseId, context }),
  leaseState: (leaseId: Uuid): Promise<LeaseState> =>
    call('lease_state', () => mock.leaseState(leaseId), { leaseId }),

  /** Reopen a lease that broke with its session; it keeps its id. */
  leaseReopen: (leaseId: Uuid): Promise<LeaseReopened> =>
    call('lease_reopen', () => mock.leaseReopen(leaseId), { leaseId }),
  /** SELECT / INSERT / UPDATE / DELETE template of a table. */
  scriptGenerate: (
    sessionId: Uuid,
    path: ObjectPath,
    template: ScriptTemplate,
  ): Promise<ScriptGenerated> =>
    call('script_generate', () => mock.scriptGenerate(sessionId, path, template), {
      sessionId,
      path,
      template,
    }),

  historyList: (query: HistoryQuery): Promise<HistoryEntry[]> =>
    call('history_list', () => mock.historyList(query), { query }),
  historyDelete: (ids: number[]): Promise<void> =>
    call('history_delete', () => mock.historyDelete(ids), { ids }),
  /** Resolves to the number of deleted entries. */
  historyClear: (connectionId?: Uuid | null): Promise<number> =>
    call('history_clear', () => mock.historyClear(connectionId ?? null), {
      connectionId: connectionId ?? null,
    }),
  savedQueriesList: (): Promise<SavedQueriesSnapshot> =>
    call('saved_queries_list', () => mock.savedQueriesList()),
  savedQuerySave: (query: SavedQuery): Promise<SavedQuery> =>
    call('saved_query_save', () => mock.savedQuerySave(query), { query }),
  savedQueryDelete: (id: Uuid): Promise<void> =>
    call('saved_query_delete', () => mock.savedQueryDelete(id), { id }),
  savedFolderSave: (folder: SavedFolder): Promise<SavedFolder> =>
    call('saved_folder_save', () => mock.savedFolderSave(folder), { folder }),
  savedFolderDelete: (id: Uuid): Promise<void> =>
    call('saved_folder_delete', () => mock.savedFolderDelete(id), { id }),
  editorTabsGet: (): Promise<EditorTabState[]> =>
    call('editor_tabs_get', () => mock.editorTabsGet()),
  editorTabsSet: (tabs: EditorTabState[]): Promise<void> =>
    call('editor_tabs_set', () => mock.editorTabsSet(tabs), { tabs }),
  /** Native open dialog; `null` when cancelled. */
  sqlFileOpen: (): Promise<SqlFile | null> => call('sql_file_open', () => mock.sqlFileOpen()),
  /** Save to `path`, or ask for one when omitted; resolves to the path (`null` = cancelled). */
  sqlFileSave: (path: string | null | undefined, text: string): Promise<string | null> =>
    call('sql_file_save', () => mock.sqlFileSave(path ?? null, text), {
      path: path ?? null,
      text,
    }),
};
