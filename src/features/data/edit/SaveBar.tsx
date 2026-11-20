import { Check, X } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { applyTarget, type EditTarget } from './editActions';
import { patchEditSession, useEditSession } from './editSession';
import { countChanges, usePending, usePendingChanges } from './pendingChanges';

interface SaveBarProps {
  tabId: string;
  target: EditTarget;
  /** Message of the confirmation before dropping the edits (default: the table one). */
  discardConfirm?: string;
}

/**
 * Save / Cancel of the edits made straight in the grid, shown under it (as in DBeaver): Save
 * writes every pending change, Cancel drops them. Both are disabled while nothing is pending.
 */
export function SaveBar({ tabId, target, discardConfirm }: SaveBarProps) {
  const { t } = useTranslation('data');
  const session = useEditSession(tabId);
  const count = countChanges(usePending(tabId));
  const [saving, setSaving] = useState(false);

  if (!target.editability.editable) return null;
  // With edit mode switched off the bar only stays for changes that are still pending.
  if (!session.editMode && count === 0) return null;

  const save = async () => {
    setSaving(true);
    await applyTarget(tabId, target);
    setSaving(false);
  };

  const cancel = () => {
    if (!window.confirm(discardConfirm ?? t('edit.discardConfirm'))) return;
    usePendingChanges.getState().discard(tabId);
    patchEditSession(tabId, { editing: null, failure: null });
  };

  return (
    <span className="flex shrink-0 items-center gap-1.5" data-testid="save-bar">
      <Button
        size="sm"
        variant="primary"
        disabled={count === 0 || saving || session.applying}
        onClick={() => void save()}
      >
        <Check size={12} /> {saving ? t('edit.saving') : t('edit.save')}
      </Button>
      <Button size="sm" disabled={count === 0 || saving || session.applying} onClick={cancel}>
        <X size={12} /> {t('edit.cancel')}
      </Button>
    </span>
  );
}
