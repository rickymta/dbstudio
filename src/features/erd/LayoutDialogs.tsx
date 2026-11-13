import { useQuery } from '@tanstack/react-query';
import { Trash2 } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { EmptyState } from '@/components/ui/EmptyState';
import { IconButton } from '@/components/ui/IconButton';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import { ipc } from '@/lib/ipc';
import type { ErdLayoutSummary, ObjectPath, Uuid } from '@/lib/types';
import { pathsEqual } from '@/lib/objectPath';
import { erdKeys } from './useErd';

interface SaveDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  initialName: string;
  /** The tab shows a saved layout: offer "save as new". */
  canSaveAsNew: boolean;
  onSave: (name: string, asNew: boolean) => Promise<unknown>;
}

export function SaveLayoutDialog({
  open,
  onOpenChange,
  initialName,
  canSaveAsNew,
  onSave,
}: SaveDialogProps) {
  const { t } = useTranslation('erd');
  const [name, setName] = useState(initialName);
  const [busy, setBusy] = useState(false);
  const submit = async (asNew: boolean) => {
    if (!name.trim()) return;
    setBusy(true);
    try {
      const saved = await onSave(name.trim(), asNew);
      if (saved) onOpenChange(false);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title={t('save.title')}
        description={t('save.description')}
        footer={
          <>
            <Button onClick={() => onOpenChange(false)}>{t('save.cancel')}</Button>
            {canSaveAsNew && (
              <Button disabled={busy || !name.trim()} onClick={() => void submit(true)}>
                {t('save.asNew')}
              </Button>
            )}
            <Button
              variant="primary"
              disabled={busy || !name.trim()}
              onClick={() => void submit(false)}
            >
              {t('save.save')}
            </Button>
          </>
        }
      >
        <form
          className="flex flex-col gap-1.5"
          onSubmit={(e) => {
            e.preventDefault();
            void submit(false);
          }}
        >
          <Label htmlFor="erd-layout-name">{t('save.name')}</Label>
          <Input
            id="erd-layout-name"
            value={name}
            maxLength={120}
            autoFocus
            onChange={(e) => setName(e.target.value)}
          />
        </form>
      </DialogContent>
    </Dialog>
  );
}

interface LayoutsDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  connectionId: Uuid;
  scope: ObjectPath;
  activeId: Uuid | null;
  onOpenLayout: (id: Uuid) => void;
  onDeleteLayout: (id: Uuid) => Promise<void>;
}

export function LayoutsDialog({
  open,
  onOpenChange,
  connectionId,
  scope,
  activeId,
  onOpenLayout,
  onDeleteLayout,
}: LayoutsDialogProps) {
  const { t } = useTranslation('erd');
  const list = useQuery<ErdLayoutSummary[]>({
    queryKey: erdKeys.layouts(connectionId),
    queryFn: () => ipc.erdLayoutsList(connectionId),
    enabled: open,
  });
  const layouts = (list.data ?? []).filter((l) => pathsEqual(l.scope, scope));
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={t('layouts.title')} description={t('layouts.description')}>
        {layouts.length === 0 ? (
          <EmptyState title={t('layouts.empty')} />
        ) : (
          <ul className="flex flex-col divide-y divide-border">
            {layouts.map((l) => (
              <li key={l.id} className="flex items-center gap-2 py-1.5">
                <div className="min-w-0 flex-1">
                  <div className="truncate text-[13px] font-medium">
                    {l.name}
                    {l.id === activeId && (
                      <span className="ml-2 text-[11px] font-normal text-accent">
                        {t('layouts.current')}
                      </span>
                    )}
                  </div>
                  <div className="text-[11px] text-muted-foreground">
                    {t('layouts.tables', { n: l.tableCount })} ·{' '}
                    {l.updatedAt.slice(0, 16).replace('T', ' ')}
                  </div>
                </div>
                <Button
                  size="sm"
                  onClick={() => {
                    onOpenLayout(l.id);
                    onOpenChange(false);
                  }}
                >
                  {t('layouts.open')}
                </Button>
                <IconButton
                  label={t('layouts.delete')}
                  icon={<Trash2 size={13} />}
                  onClick={() => void onDeleteLayout(l.id)}
                />
              </li>
            ))}
          </ul>
        )}
      </DialogContent>
    </Dialog>
  );
}
