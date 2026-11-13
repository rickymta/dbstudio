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
