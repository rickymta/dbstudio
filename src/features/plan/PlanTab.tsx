import type { FC } from 'react';
import { Workflow } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { EmptyState } from '@/components/ui/EmptyState';
import type { PlanTabPayload } from '@/stores/tabs';

/** Placeholder until this feature ships; its owner replaces the body of this component. */
export const PlanTab: FC<PlanTabPayload> = () => {
  const { t } = useTranslation('plan');
  return (
    <div className="flex h-full items-center justify-center" data-testid="plan-tab">
      <EmptyState
        icon={<Workflow size={32} />}
        title={t('placeholder.title')}
        description={t('placeholder.description')}
      />
    </div>
  );
};
