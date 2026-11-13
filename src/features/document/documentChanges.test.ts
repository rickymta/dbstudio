import { describe, expect, it } from 'vitest';
import type { ObjectPath, Value } from '@/lib/types';
import {
  buildDocumentChangeSet,
  deleteChange,
  insertChange,
  replaceChange,
  unsetFieldChange,
  updateFieldChange,
} from './documentChanges';

const path: ObjectPath = {
  segments: [
    { kind: 'database', name: 'app' },
    { kind: 'folder', name: 'collections' },
    { kind: 'collection', name: 'users' },
  ],
};
const id: Value = { t: 'ext', v: { type: 'objectId', text: '64b7f0c2a1b2c3d4e5f60718' } };

describe('documentChanges', () => {
  it('builds insert, replace and delete', () => {
    const set = buildDocumentChangeSet(path, [
      insertChange('{}'),
      replaceChange(id, '{"a":1}'),
      deleteChange(id),
    ]);
    expect(set.model).toBe('documents');
    expect(set.collection).toBe(path);
    expect(set.changes).toEqual([
      { op: 'insert', document: { kind: 'ejson', text: '{}' } },
      { op: 'replace', id, document: { kind: 'ejson', text: '{"a":1}' } },
      { op: 'delete', id },
    ]);
  });

  it('builds field updates', () => {
    expect(updateFieldChange(id, ['a', 'b'], { t: 'int', v: 1 })).toEqual({
      op: 'update',
      id,
      set: [{ path: ['a', 'b'], value: { t: 'int', v: 1 } }],
    });
    expect(unsetFieldChange(id, ['a'])).toEqual({ op: 'update', id, set: [], unset: [['a']] });
  });
});
