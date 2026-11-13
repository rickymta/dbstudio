import { mock } from '../mock';
import type { ApplyResult, ChangeSet, Editability, ObjectPath, Uuid } from '../types';
import { call } from './core';

/** Data editing: editability, change preview and apply. */
export const changesApi = {
  tableEditability: (sessionId: Uuid, path: ObjectPath): Promise<Editability> =>
    call('table_editability', () => mock.tableEditability(sessionId, path), { sessionId, path }),
  /** The statements `changesApply` would run, in order. */
  changesPreview: (sessionId: Uuid, changes: ChangeSet): Promise<string[]> =>
    call('changes_preview', () => mock.changesPreview(sessionId, changes), { sessionId, changes }),
  changesApply: (sessionId: Uuid, changes: ChangeSet): Promise<ApplyResult> =>
    call('changes_apply', () => mock.changesApply(sessionId, changes), { sessionId, changes }),
};
