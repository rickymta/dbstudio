import type { FC } from 'react';
import { useTranslation } from 'react-i18next';
import { Checkbox } from '@/components/ui/Checkbox';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import type { Breakpoint } from '@/lib/types';
import { updateBreakpoint } from './debugActions';

/** Fields of one breakpoint (Enabled, Suspend, Condition, Log message, Hit count). */
export const BreakpointForm: FC<{ scopeKey: string; breakpoint: Breakpoint }> = ({
  scopeKey,
  breakpoint: b,
}) => {
  const { t } = useTranslation('debugger');
  const update = (patch: Partial<Breakpoint>) => updateBreakpoint(scopeKey, b.id, patch);
  const text = (value: string): string | null => (value.trim() === '' ? null : value);
  return (
    <div className="flex flex-col gap-2" key={b.id}>
      <div className="flex items-center gap-2">
        <Checkbox
          id={`bp-enabled-${b.id}`}
          checked={b.enabled}
          onCheckedChange={(v) => update({ enabled: v === true })}
        />
        <Label htmlFor={`bp-enabled-${b.id}`}>{t('breakpoint.enabled')}</Label>
        <Checkbox
          id={`bp-suspend-${b.id}`}
          className="ml-3"
          checked={b.suspend}
          onCheckedChange={(v) => update({ suspend: v === true })}
        />
        <Label htmlFor={`bp-suspend-${b.id}`}>{t('breakpoint.suspend')}</Label>
      </div>
      <div className="flex flex-col gap-1">
        <Label htmlFor={`bp-cond-${b.id}`}>{t('breakpoint.condition')}</Label>
        <Input
          id={`bp-cond-${b.id}`}
          className="h-7 font-mono text-xs"
          defaultValue={b.condition ?? ''}
          placeholder={t('breakpoint.conditionHint')}
          onBlur={(e) => update({ condition: text(e.currentTarget.value) })}
        />
      </div>
      <div className="flex flex-col gap-1">
        <Label htmlFor={`bp-log-${b.id}`}>{t('breakpoint.logMessage')}</Label>
        <Input
          id={`bp-log-${b.id}`}
          className="h-7 font-mono text-xs"
          defaultValue={b.logMessage ?? ''}
          placeholder={t('breakpoint.logHint')}
          onBlur={(e) => update({ logMessage: text(e.currentTarget.value) })}
        />
      </div>
      <div className="flex flex-col gap-1">
        <Label htmlFor={`bp-hit-${b.id}`}>{t('breakpoint.hitCount')}</Label>
        <Input
          id={`bp-hit-${b.id}`}
          type="number"
          min={1}
          className="h-7 w-28 text-xs"
          defaultValue={b.hitCount ?? ''}
          onBlur={(e) => {
            const n = Number.parseInt(e.currentTarget.value, 10);
            update({ hitCount: Number.isFinite(n) && n > 1 ? n : null });
          }}
        />
      </div>
    </div>
  );
};
