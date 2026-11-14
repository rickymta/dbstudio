import { useTranslation } from 'react-i18next';
import { Select } from '@/components/ui/Select';
import { Checkbox } from '@/components/ui/Checkbox';
import { Label } from '@/components/ui/Label';
import { Button } from '@/components/ui/Button';
import {
  DEFAULT_APPEARANCE,
  type Appearance,
  type Density,
  type MonoFont,
} from '@/stores/appearance';

interface Props {
  id: string;
  value: Appearance;
  onChange: (value: Appearance) => void;
}

export function AppearanceFields({ id, value, onChange }: Props) {
  const { t } = useTranslation('settings');
  const patch = (next: Partial<Appearance>) => onChange({ ...value, ...next });
  return (
    <div className="flex flex-col gap-3">
      <div className="grid grid-cols-2 gap-3">
        <div className="flex flex-col gap-1">
          <Label htmlFor={`${id}-density`}>{t('appearance.density')}</Label>
          <Select
            id={`${id}-density`}
            value={value.density}
            onValueChange={(density) => patch({ density: density as Density })}
            options={(['compact', 'comfortable'] as const).map((v) => ({
              value: v,
              label: t(`appearance.${v}`),
            }))}
          />
        </div>
        <div className="flex flex-col gap-1">
          <Label htmlFor={`${id}-mono`}>{t('appearance.monoFont')}</Label>
          <Select
            id={`${id}-mono`}
            value={value.monoFont}
            onValueChange={(monoFont) => patch({ monoFont: monoFont as MonoFont })}
            options={[
              { value: 'system', label: t('appearance.systemFont') },
              { value: 'menlo', label: 'Menlo' },
              { value: 'consolas', label: 'Consolas' },
            ]}
          />
        </div>
      </div>
      <div className="grid grid-cols-3 gap-3">
        {(['uiFontSize', 'gridFontSize', 'editorFontSize'] as const).map((key) => (
          <div key={key} className="flex flex-col gap-1">
            <Label htmlFor={`${id}-${key}`}>{t(`appearance.${key}`)}</Label>
            <Select
              id={`${id}-${key}`}
              value={String(value[key])}
              onValueChange={(size) => patch({ [key]: Number(size) })}
              options={Array.from({ length: 13 }, (_, i) => ({
                value: String(i + 12),
                label: `${i + 12} px`,
              }))}
            />
          </div>
        ))}
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <Checkbox
          id={`${id}-ligatures`}
          checked={value.ligatures}
          onCheckedChange={(checked) => patch({ ligatures: checked === true })}
        />
        <Label htmlFor={`${id}-ligatures`}>{t('appearance.ligatures')}</Label>
        <Button
          className="ml-auto"
          size="sm"
          variant="ghost"
          onClick={() => onChange({ ...DEFAULT_APPEARANCE })}
        >
          {t('appearance.reset')}
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">{t('appearance.hint')}</p>
    </div>
  );
}
