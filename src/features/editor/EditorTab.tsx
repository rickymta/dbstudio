import type { FC } from 'react';
import { SquareTerminal } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { EmptyState } from '@/components/ui/EmptyState';
import type { EditorTabPayload } from '@/stores/tabs';

/** Placeholder until this feature ships; its owner replaces the body of this component. */
export const EditorTab: FC<EditorTabPayload> = () => {
  const { t } = useTranslation('editor');
  return (
    <div className="flex h-full items-center justify-center" data-testid="editor-tab">
      <EmptyState
        icon={<SquareTerminal size={32} />}
        title={t('placeholder.title')}
        description={t('placeholder.description')}
      />
    </div>
  );
};
