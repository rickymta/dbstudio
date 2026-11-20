// Builds the `documents` change set sent to `changesPreview` / `changesApply`.

import type {
  DocInput,
  DocumentChange,
  DocumentChangeSet,
  FieldEdit,
  FieldOriginal,
  ObjectPath,
  Value,
} from '@/lib/types';
import { fromEjson, toEjsonCompact } from './ejson';
import { docFields } from './flatten';

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

/** Outcome of comparing an edited document text with the stored document. */
export type EditPlan =
  | { kind: 'none' }
  | { kind: 'change'; change: DocumentChange }
  /** `key` is an i18n key under `document:editor`. */
  | { kind: 'error'; key: 'idChanged' | 'dollarKey' | 'convertFailed'; message?: string };

/** A changed path cannot be written as a `$set`/`$unset` path (a key has a dot or a leading `$`). */
class Unrepresentable extends Error {}

const sameValue = (a: Value, b: Value) => toEjsonCompact(a) === toEjsonCompact(b);
const badKey = (k: string) => k === '' || k.includes('.') || k.startsWith('$');

function diffDocuments(
  before: [string, Value][],
  after: [string, Value][],
  prefix: string[],
  set: FieldEdit[],
  unset: string[][],
  expected: FieldOriginal[],
): void {
  const old = new Map(before);
  const next = new Map(after);
  const touch = (k: string): string[] => {
    if (badKey(k)) throw new Unrepresentable(k);
    return [...prefix, k];
  };
  // `expected` records what each written field held when the document was loaded (null = it
  // did not exist), so the server refuses the write if someone else changed it meanwhile.
  for (const [k, v] of before) {
    if (!next.has(k)) {
      unset.push(touch(k));
      expected.push({ path: touch(k), value: v });
    }
  }
  for (const [k, v] of after) {
    const prev = old.get(k);
    if (prev === undefined) {
      set.push({ path: touch(k), value: v });
      expected.push({ path: touch(k), value: null });
    } else if (prev.t === 'document' && v.t === 'document') {
      // Both sides are documents: descend so only the changed fields are written.
      diffDocuments(prev.v, v.v, [...prefix, k], set, unset, expected);
    } else if (!sameValue(prev, v)) {
      set.push({ path: touch(k), value: v });
      expected.push({ path: touch(k), value: prev });
    }
  }
}

/**
 * Plans the save of an edited document: a field-level `update` (set/unset, with the original
 * values of those fields as `expected`) computed against the original, or a `replace` only when the diff cannot be expressed as paths. `_id` must not change.
 */
export function planDocumentEdit(original: Value, id: Value, text: string): EditPlan {
  let next: Value;
  try {
    next = fromEjson(text);
  } catch (e) {
    return { kind: 'error', key: 'convertFailed', message: (e as Error).message };
  }
  const after = docFields(next);
  const before = docFields(original);
  if (!after || !before) return { kind: 'error', key: 'convertFailed', message: 'not a document' };
  const newId = after.find(([k]) => k === '_id')?.[1];
  if (!newId || !sameValue(newId, id)) return { kind: 'error', key: 'idChanged' };
  if (sameValue(original, next)) return { kind: 'none' };

  const set: FieldEdit[] = [];
  const unset: string[][] = [];
  const expected: FieldOriginal[] = [];
  try {
    diffDocuments(before, after, [], set, unset, expected);
  } catch (e) {
    if (!(e instanceof Unrepresentable)) throw e;
    // Replace cannot carry operator-like first keys (the server would read them as operators).
    if (after.some(([k]) => k.startsWith('$'))) return { kind: 'error', key: 'dollarKey' };
    return { kind: 'change', change: replaceChange(id, text) };
  }
  if (set.length === 0 && unset.length === 0) return { kind: 'none' };
  return {
    kind: 'change',
    change: { op: 'update', id, set, ...(unset.length > 0 ? { unset } : {}), expected },
  };
}
