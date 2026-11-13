// Builds the `documents` change set sent to `changesPreview` / `changesApply`.

import type { DocInput, DocumentChange, DocumentChangeSet, ObjectPath, Value } from '@/lib/types';

const ejson = (text: string): DocInput => ({ kind: 'ejson', text });

/** Insert a new document typed as EJSON. */
export function insertChange(text: string): DocumentChange {
  return { op: 'insert', document: ejson(text) };
}

/** Replace the document with `id` by the EJSON text. */
export function replaceChange(id: Value, text: string): DocumentChange {
  return { op: 'replace', id, document: ejson(text) };
}

export function deleteChange(id: Value): DocumentChange {
  return { op: 'delete', id };
}

/** Set one (possibly nested) field, e.g. in-place cell edits. */
export function updateFieldChange(id: Value, path: string[], value: Value): DocumentChange {
  return { op: 'update', id, set: [{ path, value }] };
}

export function unsetFieldChange(id: Value, path: string[]): DocumentChange {
  return { op: 'update', id, set: [], unset: [path] };
}

export function buildDocumentChangeSet(
  collection: ObjectPath,
  changes: DocumentChange[],
): DocumentChangeSet & { model: 'documents' } {
  return { model: 'documents', collection, changes };
}
