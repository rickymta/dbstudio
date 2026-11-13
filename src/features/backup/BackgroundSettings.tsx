import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useId } from 'react';
import { useTranslation } from 'react-i18next';
import { Label } from '@/components/ui/Label';
import { Select } from '@/components/ui/Select';
import { Spinner } from '@/components/ui/Spinner';
import { Switch } from '@/components/ui/Switch';
import { describeError } from '@/features/connections/errors';
import { ipc } from '@/lib/ipc';
import type { BackgroundSettings as Settings, CloseBehavior } from '@/lib/types';
import { backgroundKey } from './queries';

const CLOSE_BEHAVIORS: CloseBehavior[] = ['auto', 'always', 'never'];

/** Run-in-background settings: close-to-tray behavior and autostart. */
export function BackgroundSettings() {
  const { t } = useTranslation('backup');
  const id = useId();
  const qc = useQueryClient();
  const query = useQuery({ queryKey: backgroundKey, queryFn: ipc.backgroundGet });
  const save = useMutation({
    mutationFn: (s: Settings) => ipc.backgroundSet(s),
    onSuccess: (s) => qc.setQueryData(backgroundKey, s),
  });
  const current = query.data;

  if (query.isPending) return <Spinner className="m-3" />;
  if (!current)
    return (
      <p role="alert" className="p-3 text-xs text-danger">
        {describeError(t, query.error)}
      </p>
    );

  return (
    <div className="flex max-w-xl flex-col gap-4 p-3" data-testid="background-settings">
      <p className="text-xs text-muted-foreground">{t('background.description')}</p>
      <div className="flex flex-col gap-1">
        <Label htmlFor={`${id}-close`}>{t('background.closeToTray.label')}</Label>
        <Select
          id={`${id}-close`}
          value={current.closeToTray}
          onValueChange={(v) => save.mutate({ ...current, closeToTray: v as CloseBehavior })}
          options={CLOSE_BEHAVIORS.map((b) => ({
            value: b,
            label: t(`background.closeToTray.${b}`),
          }))}
        />
        <p className="text-[11px] text-muted-foreground">{t('background.closeToTray.hint')}</p>
      </div>
      <label className="flex items-center gap-2 text-[13px]">
        <Switch
          checked={current.autostart}
          onCheckedChange={(v) => save.mutate({ ...current, autostart: v })}
          aria-label={t('background.autostart')}
        />
        {t('background.autostart')}
      </label>
      {save.isError && (
        <p role="alert" className="text-xs text-danger">
          {describeError(t, save.error)}
        </p>
      )}
    </div>
  );
}
