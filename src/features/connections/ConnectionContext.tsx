import { Lock } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';
import { connectionColor, useConnectionProfile } from './queries';

/** Keep execution context adjacent to the controls that act on it. */
export function ConnectionContext({ connectionId }: { connectionId: string }) {
  const { t } = useTranslation('connections');
  const profile = useConnectionProfile(connectionId);
  if (!profile) return null;
  const color = connectionColor(profile);
  return (
    <div className="flex min-w-0 items-center gap-2 text-[13px]" data-testid="connection-context">
      <span
        aria-hidden
        className="size-2.5 shrink-0 rounded-full bg-border"
        style={color ? { backgroundColor: color } : undefined}
      />
      <span className="max-w-60 truncate font-medium" title={`${profile.name} · ${profile.host}`}>
        {profile.name}
      </span>
      {profile.environment && (
        <span
          className={cn(
            'shrink-0 rounded px-1.5 py-0.5 text-xs',
            profile.environment === 'production'
              ? 'bg-danger-soft text-danger'
              : profile.environment === 'staging'
                ? 'bg-warning-soft text-warning'
                : 'bg-surface-muted text-muted-foreground',
          )}
        >
          {t(`environment.${profile.environment}`)}
        </span>
      )}
      {profile.readOnly && (
        <span className="flex shrink-0 items-center gap-1 text-xs text-muted-foreground">
          <Lock size={13} />
          {t('row.readOnly')}
        </span>
      )}
    </div>
  );
}
