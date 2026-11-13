import { Loader2 } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';

export function Spinner({ size = 14, className }: { size?: number; className?: string }) {
  const { t } = useTranslation();
  return (
    <Loader2
      size={size}
      role="status"
      aria-label={t('spinner.loading')}
      className={cn('animate-spin text-muted-foreground', className)}
    />
  );
}
