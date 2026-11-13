import type { DdlTabPayload } from '@/stores/tabs';
import { DdlViewer } from './DdlViewer';
import { ObjectHeader } from './ObjectHeader';

/** Standalone DDL tab (`ddl` tab kind). */
export function DdlTab({ sessionId, connectionId, path }: DdlTabPayload) {
  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="ddl-tab">
      <ObjectHeader connectionId={connectionId} path={path} />
      <div className="min-h-0 flex-1">
        <DdlViewer sessionId={sessionId} connectionId={connectionId} path={path} />
      </div>
    </div>
  );
}
