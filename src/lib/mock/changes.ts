// Mock data editing: nothing is editable in the browser mock.

import type { ApplyResult, ChangeSet, Editability, ObjectPath, Uuid } from '../types';
import { mongoApply, mongoEditability, mongoPreview } from './catalogMongo';
import { mockSessionKind } from './sessions';
import { unsupported } from './util';

export const changesMock = {
  async tableEditability(sessionId: Uuid, path: ObjectPath): Promise<Editability> {
    if (mockSessionKind(sessionId) === 'mongodb') return mongoEditability(path);
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
    if (changes.model === 'documents') return mongoPreview(changes.collection, changes.changes);
    return [];
  },
  async changesApply(sessionId: Uuid, changes: ChangeSet): Promise<ApplyResult> {
    mockSessionKind(sessionId);
    if (changes.model === 'documents') return mongoApply(changes.collection, changes.changes);
    throw unsupported('data_editing');
  },
};
