import type { FC } from 'react';
import { Network } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { EmptyState } from '@/components/ui/EmptyState';
import type { ErdTabPayload } from '@/stores/tabs';

/** Placeholder until this feature ships; its owner replaces the body of this component. */
export const ErdTab: FC<ErdTabPayload> = () => {
  const { t } = useTranslation('erd');
  return (
    <div className="flex h-full items-center justify-center" data-testid="erd-tab">
      <EmptyState
        icon={<Network size={32} />}
        title={t('placeholder.title')}
        description={t('placeholder.description')}
      />
    </div>
  );
};
