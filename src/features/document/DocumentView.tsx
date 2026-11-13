import type { FC } from 'react';
import { FileJson } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { EmptyState } from '@/components/ui/EmptyState';
import type { TableTabPayload } from '@/stores/tabs';

/** Placeholder until this feature ships; its owner replaces the body of this component. */
export const DocumentView: FC<TableTabPayload> = () => {
  const { t } = useTranslation('document');
  return (
    <div className="flex h-full items-center justify-center" data-testid="document-view">
      <EmptyState
        icon={<FileJson size={32} />}
        title={t('placeholder.title')}
        description={t('placeholder.description')}
      />
    </div>
  );
};
