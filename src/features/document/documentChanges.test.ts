import { describe, expect, it } from 'vitest';
import type { ObjectPath, Value } from '@/lib/types';
import { fromEjson } from './ejson';
import {
  buildDocumentChangeSet,
  deleteChange,
  insertChange,
  planDocumentEdit,
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

describe('planDocumentEdit', () => {
  const orig = fromEjson(
    '{"_id": {"$oid": "64b7f0c2a1b2c3d4e5f60718"}, "a": 1, "b": {"c": 1, "d": [1, 2]}, "gone": true}',
  );
  const plan = (text: string) => planDocumentEdit(orig, id, text);
  const head = '"_id": {"$oid": "64b7f0c2a1b2c3d4e5f60718"}';

  it('reports no change for identical content', () => {
    expect(plan(`{${head}, "a": 1, "b": {"c": 1, "d": [1, 2]}, "gone": true}`)).toEqual({
      kind: 'none',
    });
  });

  it('diffs first-level keys and descends into nested documents', () => {
    const r = plan(`{${head}, "a": 2, "b": {"c": 1, "d": [1, 3], "e": null}, "n": "x"}`);
    expect(r).toEqual({
      kind: 'change',
      change: {
        op: 'update',
        id,
        set: [
          { path: ['a'], value: { t: 'int', v: 2 } },
          {
            path: ['b', 'd'],
            value: {
              t: 'array',
              v: [
                { t: 'int', v: 1 },
                { t: 'int', v: 3 },
              ],
            },
          },
          { path: ['b', 'e'], value: { t: 'null' } },
          { path: ['n'], value: { t: 'text', v: 'x' } },
        ],
        unset: [['gone']],
        // What each written field held when the document was loaded (null = absent).
        expected: [
          { path: ['gone'], value: { t: 'bool', v: true } },
          { path: ['a'], value: { t: 'int', v: 1 } },
          {
            path: ['b', 'd'],
            value: {
              t: 'array',
              v: [
                { t: 'int', v: 1 },
                { t: 'int', v: 2 },
              ],
            },
          },
          { path: ['b', 'e'], value: null },
          { path: ['n'], value: null },
        ],
      },
    });
  });

  it('rejects a changed or missing _id', () => {
    expect(plan('{"_id": 5, "a": 1}')).toMatchObject({ kind: 'error', key: 'idChanged' });
    expect(plan('{"a": 1}')).toMatchObject({ kind: 'error', key: 'idChanged' });
  });

  it('falls back to replace when a changed key has a dot', () => {
    const r = plan(`{${head}, "a": 1, "x.y": 2}`);
    expect(r.kind).toBe('change');
    if (r.kind === 'change') expect(r.change.op).toBe('replace');
  });

  it('blocks a replace whose top-level key starts with $', () => {
    expect(plan(`{${head}, "$set": 2}`)).toMatchObject({ kind: 'error', key: 'dollarKey' });
  });

  it('reports values that cannot be converted', () => {
    const r = plan(`{${head}, "p": {"$dbPointer": {}}}`);
    expect(r).toMatchObject({ kind: 'error', key: 'convertFailed' });
  });
});
