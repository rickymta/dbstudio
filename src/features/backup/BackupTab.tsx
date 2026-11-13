import { DatabaseBackup } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { EmptyState } from '@/components/ui/EmptyState';

/** Placeholder until this feature ships; its owner replaces the body of this component. */
export function BackupTab() {
  const { t } = useTranslation('backup');
  return (
    <div className="flex h-full items-center justify-center" data-testid="backup-tab">
      <EmptyState
        icon={<DatabaseBackup size={32} />}
        title={t('placeholder.title')}
        description={t('placeholder.description')}
      />
    </div>
  );
}
