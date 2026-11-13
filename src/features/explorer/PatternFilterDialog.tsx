import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';

interface PatternFilterDialogProps {
  open: boolean;
  /** Label of the node being filtered. */
  title: string;
  /** Pattern in effect (or the suggested one). */
  initial: string;
  onOpenChange: (open: boolean) => void;
  /** Empty pattern clears the filter. */
  onApply: (pattern: string) => void;
}

/** Asks for the glob pattern that filters the children of a paged tree level. */
export function PatternFilterDialog({
  open,
  title,
  initial,
  onOpenChange,
  onApply,
}: PatternFilterDialogProps) {
  const { t } = useTranslation('explorer');
  const [pattern, setPattern] = useState(initial);
  const submit = (value: string) => {
    onApply(value);
    onOpenChange(false);
  };
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title={t('pattern.title', { name: title })}
        description={t('pattern.description')}
        className="w-[min(420px,92vw)]"
        footer={
          <>
            <Button onClick={() => submit('')}>{t('pattern.clear')}</Button>
            <Button variant="primary" onClick={() => submit(pattern)}>
              {t('pattern.apply')}
            </Button>
          </>
        }
      >
        <Input
          autoFocus
          aria-label={t('pattern.label')}
          placeholder="user:*"
          value={pattern}
          onChange={(e) => setPattern(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') submit(pattern);
          }}
        />
      </DialogContent>
    </Dialog>
  );
}
