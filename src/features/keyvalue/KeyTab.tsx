import type { FC } from 'react';
import { KeyRound } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { EmptyState } from '@/components/ui/EmptyState';
import type { KeyTabPayload } from '@/stores/tabs';

/** Placeholder until this feature ships; its owner replaces the body of this component. */
export const KeyTab: FC<KeyTabPayload> = () => {
  const { t } = useTranslation('keyvalue');
  return (
    <div className="flex h-full items-center justify-center" data-testid="key-tab">
      <EmptyState
        icon={<KeyRound size={32} />}
        title={t('placeholder.title')}
        description={t('placeholder.description')}
      />
    </div>
  );
};
