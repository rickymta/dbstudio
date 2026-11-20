import { useId } from 'react';
import { useTranslation } from 'react-i18next';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import { Select } from '@/components/ui/Select';
import { Switch } from '@/components/ui/Switch';
import type { DbKind } from '@/lib/types';
import {
  kindOptionsFor,
  readKindOption,
  writeKindOption,
  type KindOption,
} from './kindOptionFields';

/** Fields of the driver options that are specific to one database kind (Cassandra, ClickHouse...). */
export function KindOptions({
  kind,
  value,
  onChange,
}: {
  kind: DbKind;
  value: Record<string, string>;
  onChange: (options: Record<string, string>) => void;
}) {
  const { t } = useTranslation('connections');
  const id = useId();
  const fields = kindOptionsFor(kind);
  if (fields.length === 0) return null;

  const set = (option: KindOption, next: string | boolean) =>
    onChange(writeKindOption(value, option, next));

  return (
    <div className="flex flex-col gap-2.5" data-testid="kind-options">
      {fields.map((option) => {
        const fieldId = `${id}-${option.id}`;
        const label = t(`kindOption.${option.id}.label`);
        const hint = t(`kindOption.${option.id}.hint`, { defaultValue: '' });
        const current = readKindOption(value, option);
        if (option.type === 'switch') {
          return (
            <div key={option.id} className="flex items-start gap-2">
              <Switch
                id={fieldId}
                checked={current === true}
                onCheckedChange={(v) => set(option, v)}
                className="mt-0.5"
              />
              <div className="min-w-0">
                <Label htmlFor={fieldId} className="text-[13px] font-normal text-foreground">
                  {label}
                </Label>
                {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
              </div>
            </div>
          );
        }
        return (
          <div key={option.id} className="flex min-w-0 flex-col gap-1">
            <Label htmlFor={fieldId}>{label}</Label>
            {option.type === 'text' ? (
              <Input
                id={fieldId}
                spellCheck={false}
                value={String(current)}
                onChange={(e) => set(option, e.target.value)}
              />
            ) : (
              <Select
                id={fieldId}
                value={String(current)}
                onValueChange={(v) => set(option, v)}
                options={[
                  ...option.values,
                  // A value typed elsewhere (pasted URI) that this list does not know stays visible.
                  ...(option.values.includes(String(current)) ? [] : [String(current)]),
                ].map((v) => ({
                  value: v,
                  label:
                    option.rawLabels || !option.values.includes(v)
                      ? v
                      : t(`kindOption.${option.id}.value.${v}`),
                }))}
              />
            )}
            {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
          </div>
        );
      })}
    </div>
  );
}
