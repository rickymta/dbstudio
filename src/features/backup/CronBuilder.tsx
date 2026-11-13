import { useId, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import { Select } from '@/components/ui/Select';
import { Spinner } from '@/components/ui/Spinner';
import { cn } from '@/lib/cn';
import { describeError } from '@/features/connections/errors';
import type { ScheduleTz } from '@/lib/types';
import { formatDateTime } from './format';
import { PREVIEW_COUNT, useSchedulePreview } from './queries';
import {
  buildCron,
  DEFAULT_PRESET,
  describeCron,
  isCronShape,
  parseCron,
  type PresetKind,
  type SchedulePreset,
} from './schedule';

const PRESET_KINDS: PresetKind[] = ['daily', 'weekly', 'monthly'];

export interface CronBuilderProps {
  cron: string;
  timezone: ScheduleTz;
  onCronChange: (cron: string) => void;
  onTimezoneChange: (tz: ScheduleTz) => void;
  /** Validation error code already computed by the form (`cronInvalid`). */
  invalid?: boolean;
}

function clampInt(text: string, min: number, max: number, fallback: number): number {
  const n = Number.parseInt(text, 10);
  return Number.isNaN(n) ? fallback : Math.min(max, Math.max(min, n));
}

/** Schedule editor: daily/weekly/monthly presets or a raw 5-field cron, with a live preview. */
export function CronBuilder({
  cron,
  timezone,
  onCronChange,
  onTimezoneChange,
  invalid,
}: CronBuilderProps) {
  const { t, i18n } = useTranslation('backup');
  const id = useId();
  const [advanced, setAdvanced] = useState(() => parseCron(cron) === null);
  const preset = parseCron(cron) ?? DEFAULT_PRESET;
  const preview = useSchedulePreview(cron, timezone);
  const shapeOk = isCronShape(cron);
  const description = shapeOk ? describeCron(cron, i18n.language) : null;

  const update = (patch: Partial<SchedulePreset>) =>
    onCronChange(buildCron({ ...preset, ...patch }));
  const toggleDay = (d: number) => {
    const set = new Set(preset.weekdays);
    if (set.has(d)) set.delete(d);
    else set.add(d);
    update({ weekdays: [...set] });
  };

  return (
    <div className="flex flex-col gap-3" data-testid="cron-builder">
      <div className="flex gap-2" role="group" aria-label={t('cron.mode.label')}>
        {[false, true].map((adv) => (
          <button
            key={String(adv)}
            type="button"
            aria-pressed={advanced === adv}
            onClick={() => {
              setAdvanced(adv);
              if (!adv && parseCron(cron) === null) onCronChange(buildCron(DEFAULT_PRESET));
            }}
            className={cn(
              'rounded-md border px-2 py-1 text-xs',
              advanced === adv
                ? 'border-accent bg-accent/10 text-foreground'
                : 'border-border text-muted-foreground hover:bg-surface-muted',
            )}
          >
            {t(adv ? 'cron.mode.advanced' : 'cron.mode.preset')}
          </button>
        ))}
      </div>

      {advanced ? (
        <div className="flex flex-col gap-1">
          <Label htmlFor={`${id}-cron`}>{t('cron.expression')}</Label>
          <Input
            id={`${id}-cron`}
            value={cron}
            spellCheck={false}
            className="font-mono"
            aria-invalid={invalid || undefined}
            onChange={(e) => onCronChange(e.target.value)}
            placeholder="0 2 * * *"
          />
          <p className="text-[11px] text-muted-foreground">{t('cron.expressionHint')}</p>
        </div>
      ) : (
        <div className="flex flex-col gap-2">
          <div className="grid grid-cols-[1fr_auto_auto] items-end gap-2">
            <div className="flex flex-col gap-1">
              <Label htmlFor={`${id}-kind`}>{t('cron.repeat')}</Label>
              <Select
                id={`${id}-kind`}
                value={preset.kind}
                onValueChange={(v) => update({ kind: v as PresetKind })}
                options={PRESET_KINDS.map((k) => ({ value: k, label: t(`cron.kind.${k}`) }))}
              />
            </div>
            <div className="flex flex-col gap-1">
              <Label htmlFor={`${id}-hour`}>{t('cron.hour')}</Label>
              <Input
                id={`${id}-hour`}
                type="number"
                min={0}
                max={23}
                className="w-16"
                value={preset.hour}
                onChange={(e) => update({ hour: clampInt(e.target.value, 0, 23, preset.hour) })}
              />
            </div>
            <div className="flex flex-col gap-1">
              <Label htmlFor={`${id}-minute`}>{t('cron.minute')}</Label>
              <Input
                id={`${id}-minute`}
                type="number"
                min={0}
                max={59}
                className="w-16"
                value={preset.minute}
                onChange={(e) => update({ minute: clampInt(e.target.value, 0, 59, preset.minute) })}
              />
            </div>
          </div>
          {preset.kind === 'weekly' && (
            <div className="flex flex-col gap-1">
              <Label>{t('cron.weekdays')}</Label>
              <div className="flex flex-wrap gap-1">
                {[1, 2, 3, 4, 5, 6, 0].map((d) => (
                  <button
                    key={d}
                    type="button"
                    aria-pressed={preset.weekdays.includes(d)}
                    onClick={() => toggleDay(d)}
                    className={cn(
                      'h-7 min-w-9 rounded-md border px-2 text-xs',
                      preset.weekdays.includes(d)
                        ? 'border-accent bg-accent text-accent-foreground'
                        : 'border-border hover:bg-surface-muted',
                    )}
                  >
                    {t(`cron.weekday.${d}`)}
                  </button>
                ))}
              </div>
            </div>
          )}
          {preset.kind === 'monthly' && (
            <div className="flex flex-col gap-1">
              <Label htmlFor={`${id}-dom`}>{t('cron.dayOfMonth')}</Label>
              <Input
                id={`${id}-dom`}
                type="number"
                min={1}
                max={31}
                className="w-20"
                value={preset.day}
                onChange={(e) => update({ day: clampInt(e.target.value, 1, 31, preset.day) })}
              />
            </div>
          )}
          <p className="font-mono text-[11px] text-muted-foreground" data-testid="cron-expression">
            {cron}
          </p>
        </div>
      )}

      <div className="flex flex-col gap-1">
        <Label htmlFor={`${id}-tz`}>{t('cron.timezone')}</Label>
        <Select
          id={`${id}-tz`}
          value={timezone}
          onValueChange={(v) => onTimezoneChange(v as ScheduleTz)}
          options={(['local', 'utc'] as const).map((z) => ({
            value: z,
            label: t(`cron.tz.${z}`),
          }))}
        />
      </div>

      {!shapeOk ? (
        <p role="alert" className="rounded-md bg-danger-soft px-2 py-1.5 text-xs text-danger">
          {t('cron.invalid')}
        </p>
      ) : (
        <>
          {description && (
            <p className="text-[13px]" data-testid="cron-description">
              {description}
            </p>
          )}
          {preview.isPending && <Spinner size={12} />}
          {preview.isError && (
            <p role="alert" className="rounded-md bg-danger-soft px-2 py-1.5 text-xs text-danger">
              {describeError(t, preview.error)}
            </p>
          )}
          {preview.data && (
            <div>
              <p className="mb-1 text-xs font-medium text-muted-foreground">
                {t('cron.nextRuns', { count: PREVIEW_COUNT })}
              </p>
              <ol className="flex flex-col gap-0.5 text-xs" data-testid="cron-preview">
                {preview.data.map((iso) => (
                  <li key={iso}>{formatDateTime(iso, i18n.language)}</li>
                ))}
              </ol>
            </div>
          )}
        </>
      )}
    </div>
  );
}
