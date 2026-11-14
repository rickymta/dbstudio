import { Star, Clock } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { useConnectionsSnapshot } from '@/features/connections/queries';
import { useNavigation } from './navigation';

export function QuickAccess() {
  const { t } = useTranslation();
  const recent = useNavigation((s) => s.recent);
  const favorites = useNavigation((s) => s.favorites);
  const navigate = useNavigation((s) => s.navigate);
  const { data } = useConnectionsSnapshot();
  return (
    <div className="max-h-[28vh] shrink-0 overflow-auto border-b border-border px-2 py-1">
      {(
        [
          ['favorites', favorites, Star],
          ['recent', recent.slice(0, 5), Clock],
        ] as const
      ).map(([key, items, Icon]) => {
        const visible = items.filter((x) => data?.profiles.some((p) => p.id === x.connectionId));
        if (!visible.length) return null;
        return (
          <details key={key} open={key === 'favorites'}>
            <summary className="cursor-pointer py-1 text-xs text-muted-foreground">
              {t(`workspace.${key}`)}
            </summary>
            {visible.map((object) => {
              const profile = data?.profiles.find((p) => p.id === object.connectionId);
              const context = [
                profile?.name,
                ...object.path.segments.filter((s) => s.kind !== 'folder').map((s) => s.name),
              ].join(' › ');
              return (
                <button
                  key={object.key}
                  className="flex min-h-8 w-full items-center gap-2 rounded px-2 text-left hover:bg-surface-muted"
                  title={context}
                  onClick={() => navigate(object.connectionId, object)}
                >
                  <Icon size={13} className="shrink-0 text-muted-foreground" />
                  <span className="min-w-0 flex-1 truncate">{object.name}</span>
                  <span className="max-w-[35%] truncate text-xs text-muted-foreground">
                    {profile?.name}
                  </span>
                </button>
              );
            })}
          </details>
        );
      })}
    </div>
  );
}
