import { ChevronRight } from 'lucide-react';
import { Fragment } from 'react';
import { useConnectionProfile, connectionColor } from '@/features/connections/queries';
import type { ObjectPath, Uuid } from '@/lib/types';
import { displaySegments } from './objectLabel';

/** Connection color dot + `database > schema > table`. */
export function ObjectHeader({ connectionId, path }: { connectionId: Uuid; path: ObjectPath }) {
  const profile = useConnectionProfile(connectionId);
  const color = connectionColor(profile);
  const names = displaySegments(path);
  return (
    <div className="flex h-8 shrink-0 items-center gap-2 border-b border-border px-3 text-[13px]">
      <span
        aria-hidden
        title={profile?.name}
        className="size-2.5 shrink-0 rounded-full bg-border"
        style={color ? { backgroundColor: color } : undefined}
      />
      <nav aria-label="path" className="flex min-w-0 items-center gap-0.5 truncate">
        {names.map((n, i) => (
          <Fragment key={i}>
            {i > 0 && <ChevronRight size={12} className="shrink-0 text-muted-foreground" />}
            <span className={i === names.length - 1 ? 'font-medium' : 'text-muted-foreground'}>
              {n}
            </span>
          </Fragment>
        ))}
      </nav>
    </div>
  );
}
