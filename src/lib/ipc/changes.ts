import { mock } from '../mock';
import type {
  ApplyResult,
  ChangeSet,
  ColumnMeta,
  Editability,
  ObjectPath,
  ResultEditability,
  Uuid,
} from '../types';
import { call } from './core';

/** Data editing: editability, change preview and apply. */
export const changesApi = {
  tableEditability: (sessionId: Uuid, path: ObjectPath): Promise<Editability> =>
    call('table_editability', () => mock.tableEditability(sessionId, path), { sessionId, path }),
  /** The statements `changesApply` would run, in order. */
  changesPreview: (sessionId: Uuid, changes: ChangeSet): Promise<string[]> =>
    call('changes_preview', () => mock.changesPreview(sessionId, changes), { sessionId, changes }),
  /**
   * Apply a change set. With `leaseId` (an editor tab's connection) and a transaction open on it,
   * the changes run inside that transaction instead of committing on their own.
   */
  changesApply: (
    sessionId: Uuid,
    changes: ChangeSet,
    leaseId?: Uuid | null,
  ): Promise<ApplyResult> =>
    call('changes_apply', () => mock.changesApply(sessionId, changes), {
      sessionId,
      changes,
      leaseId: leaseId ?? null,
    }),
  /** Whether the rows of a query result can be edited, and by which table and key. */
  resultEditability: (
    sessionId: Uuid,
    leaseId: Uuid | null,
    columns: ColumnMeta[],
  ): Promise<ResultEditability> =>
    call('result_editability', () => mock.resultEditability(sessionId, columns), {
      sessionId,
      leaseId,
      columns,
    }),
};
