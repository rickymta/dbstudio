import { Database, Plus } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Tooltip } from '@/components/ui/Tooltip';

/** Left sidebar. Phase 0: only the empty state; real connections arrive in phase 1. */
export function ConnectionsSidebar() {
  const { t } = useTranslation();
  return (
    <aside className="flex h-full flex-col bg-surface" aria-label={t('sidebar.connections')}>
      <header className="flex h-9 shrink-0 items-center justify-between border-b border-border px-3">
        <h2 className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
          {t('sidebar.connections')}
        </h2>
      </header>
      <div className="flex flex-1 flex-col items-center justify-center gap-3 p-4 text-center">
        <Database size={28} className="text-muted-foreground" aria-hidden />
        <div>
          <p className="font-medium">{t('sidebar.empty')}</p>
          <p className="mt-1 text-xs text-muted-foreground">{t('sidebar.emptyHint')}</p>
        </div>
        <Tooltip content={t('sidebar.availableInPhase', { phase: 1 })}>
          <Button variant="primary" disabled>
            <Plus size={14} />
            {t('sidebar.newConnection')}
          </Button>
        </Tooltip>
      </div>
    </aside>
  );
}
