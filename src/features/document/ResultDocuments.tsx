import type { ReactNode } from 'react';
import type { StatementResult } from '@/features/editor/results';
import type { Value } from '@/lib/types';
import { toEjson } from './ejson';

const MAX_DOCUMENTS = 200;

/** Documents of a result drawn as pretty Extended JSON, one block per document. */
export function ResultDocuments({ docs }: { docs: Value[] }) {
  return (
    <div className="flex flex-col gap-2 p-2" data-testid="mongo-documents">
      {docs.slice(0, MAX_DOCUMENTS).map((doc, i) => (
        <pre
          key={i}
          className="rounded border border-border bg-surface-muted p-2 font-mono text-xs break-words whitespace-pre-wrap select-text"
        >
          {toEjson(doc)}
        </pre>
      ))}
    </div>
  );
}

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
