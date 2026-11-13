import { DocumentView } from '@/features/document/DocumentView';
import { TableTab } from '@/features/data/TableTab';
import { useSessionsStore } from '@/stores/sessions';
import type { TableTabPayload } from '@/stores/tabs';

/** Document stores browse collections in `DocumentView`; everything else uses the grid tab. */
export function ObjectDataTab(payload: TableTabPayload) {
  const documentStore = useSessionsStore((s) =>
    Object.values(s.sessionsByConnection).some(
      (x) => x.sessionId === payload.sessionId && x.descriptor.capabilities.documentStore,
    ),
  );
  return documentStore ? <DocumentView {...payload} /> : <TableTab {...payload} />;
}
