import type { ReactNode } from 'react';
import type { StatementResult } from '@/features/editor/results';
import type { Value } from '@/lib/types';
import { ResultDocumentTable } from './ResultDocumentTable';
import { ResultDocuments } from './ResultDocuments';

/** The documents of a result with a single document column, or `null` for any other shape. */
export function resultDocuments(result: StatementResult): Value[] | null {
  const model = result.model;
  if (result.error || !model || model.rows.length === 0) return null;
  if (model.columns.length !== 1 || model.columns[0]!.valueKind !== 'document') return null;
  return model.rows.map((r) => r[0]!).filter((v) => v.t === 'document');
}

/** Renderer registered for the MongoDB mode (console plug-in and the result view toggle). */
export function renderMongoResult(result: StatementResult): ReactNode | null {
  const docs = resultDocuments(result);
  return docs ? <ResultDocuments docs={docs} /> : null;
}

/** Table view of a document result: one column per first-level key instead of `$document`. */
export function renderMongoTable(result: StatementResult): ReactNode | null {
  return resultDocuments(result) && result.model ? (
    <ResultDocumentTable model={result.model} />
  ) : null;
}
