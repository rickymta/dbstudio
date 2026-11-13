// Mock data editing: nothing is editable in the browser mock.

import type { ApplyResult, ChangeSet, Editability, ObjectPath, Uuid } from '../types';
import { mockSessionKind } from './sessions';
import { unsupported } from './util';

export const changesMock = {
  async tableEditability(sessionId: Uuid, path: ObjectPath): Promise<Editability> {
    mockSessionKind(sessionId);
    void path;
    return {
      editable: false,
      keyColumns: [],
      readOnlyColumns: [],
      reason: 'unsupported',
      atomic: true,
    };
  },
  async changesPreview(sessionId: Uuid, changes: ChangeSet): Promise<string[]> {
    mockSessionKind(sessionId);
    void changes;
    return [];
  },
  async changesApply(sessionId: Uuid, changes: ChangeSet): Promise<ApplyResult> {
    mockSessionKind(sessionId);
    void changes;
    throw unsupported('data_editing');
  },
};
