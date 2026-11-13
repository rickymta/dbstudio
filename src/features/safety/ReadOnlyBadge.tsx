import { Lock } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/Badge';

/** "Read-only" marker for connections that block writes (CONN-10). */
export function ReadOnlyBadge({ readOnly }: { readOnly: boolean }) {
  const { t } = useTranslation('safety');
  if (!readOnly) return null;
  return (
    <Badge tone="warning">
      <Lock size={11} aria-hidden /> {t('readOnly')}
    </Badge>
  );
}
