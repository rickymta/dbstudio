import { useMemo, useState } from 'react';
import type { GridModel } from '@/features/data/gridModel';
import { useGridVersion } from '@/features/data/useGridVersion';
import { useAppSettings } from '@/features/settings/queries';
import { DocumentTable } from './DocumentTable';
import { ColumnTracker } from './flatten';
import { idKey } from './store';

/**
 * Documents of an editor result as a flat table: first-level keys become columns (inferred from
 * the rows loaded so far, like the collection tab), a key a document lacks stays an empty cell.
 * Read-only and unsorted: the query text decides the order (`ResultGridModel` ignores sorting)
 * and scrolling to the end still loads the next batch of the lease.
 */
export function ResultDocumentTable({ model }: { model: GridModel }) {
  const settings = useAppSettings();
  const version = useGridVersion(model);
  const [tracker] = useState(() => new ColumnTracker());
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const columns = useMemo(() => {
    tracker.update(model.rows);
    return tracker.columns.map((c) => ({ ...c, sortable: false }));
  }, [model, version]); // eslint-disable-line react-hooks/exhaustive-deps
  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="mongo-table">
      <DocumentTable
        model={model}
        columns={columns}
        selectedId={selectedId}
        timestamp={settings.timestampDisplay}
        onSelect={(i) => setSelectedId(idKey(model.rows[i]?.[0]))}
        onOpen={(i) => setSelectedId(idKey(model.rows[i]?.[0]))}
      />
    </div>
  );
}
