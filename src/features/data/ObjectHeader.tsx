import { IconButton } from '@/components/ui/IconButton';
import { useTabsStore } from '@/stores/tabs';
import { savedObject, useNavigation } from '@/features/workspace/navigation';
import { ChevronRight, Star } from 'lucide-react';
import { Fragment } from 'react';
import { useTranslation } from 'react-i18next';
import { ConnectionContext } from '@/features/connections/ConnectionContext';
import type { ObjectPath, Uuid } from '@/lib/types';
import { displaySegments } from './objectLabel';

/** Connection color dot + `database > schema > table`. */
export function ObjectHeader({ connectionId, path }: { connectionId: Uuid; path: ObjectPath }) {
  const { t } = useTranslation('data');
  const names = displaySegments(path);
  const tab = useTabsStore((s) => s.tabs.find((tab) => tab.id === s.activeId));
  const object = tab ? savedObject(tab) : null;
  const favorites = useNavigation((s) => s.favorites);
  const favorite = object && favorites.some((x) => x.key === object.key);
  return (
    <div className="flex min-h-10 flex-wrap py-1 shrink-0 items-center gap-2 border-b border-border px-3 text-[13px]">
      <ConnectionContext connectionId={connectionId} />
      <span className="h-4 w-px shrink-0 bg-border" aria-hidden />
      <nav
        aria-label={t('header.path')}
        className="flex min-w-0 items-center gap-1 overflow-x-auto"
      >
        {names.map((n, i) => (
          <Fragment key={i}>
            {i > 0 && <ChevronRight size={12} className="shrink-0 text-muted-foreground" />}
            <span className={i === names.length - 1 ? 'font-medium' : 'text-muted-foreground'}>
              {n}
            </span>
          </Fragment>
        ))}
      </nav>
      {object && (
        <div className="ml-auto">
          <IconButton
            label={t(favorite ? 'common:workspace.removeFavorite' : 'common:workspace.addFavorite')}
            icon={<Star size={15} className={favorite ? 'fill-current text-accent' : ''} />}
            aria-pressed={!!favorite}
            onClick={() => useNavigation.getState().toggle(object)}
          />
        </div>
      )}
    </div>
  );
}
