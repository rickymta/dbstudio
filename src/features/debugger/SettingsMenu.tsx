import { Check, Circle } from 'lucide-react';
import { DropdownMenu as Menu } from 'radix-ui';
import type { FC, ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Tooltip } from '@/components/ui/Tooltip';
import { menuContentClass, menuItemClass, menuSeparatorClass } from '@/components/ui/menuStyles';
import type { DebugEndAction, DebugKeymap, DebugTxMode } from '@/lib/types';
import { saveKeymap } from './debugActions';
import { useDebugStore } from './debugStore';

const itemClass = `${menuItemClass} pl-6 relative`;

/** Radio group row with a dot indicator. */
const RadioRow: FC<{ value: string; children: ReactNode }> = ({ value, children }) => (
  <Menu.RadioItem value={value} className={itemClass}>
    <Menu.ItemIndicator className="absolute left-1.5">
      <Circle size={7} fill="currentColor" aria-hidden />
    </Menu.ItemIndicator>
    {children}
  </Menu.RadioItem>
);

const CheckRow: FC<{
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
  children: ReactNode;
}> = ({ checked, onCheckedChange, children }) => (
  <Menu.CheckboxItem
    checked={checked}
    onCheckedChange={onCheckedChange}
    className={itemClass}
    onSelect={(e) => e.preventDefault()}
  >
    <Menu.ItemIndicator className="absolute left-1.5">
      <Check size={12} aria-hidden />
    </Menu.ItemIndicator>
    {children}
  </Menu.CheckboxItem>
);

const Label: FC<{ children: ReactNode }> = ({ children }) => (
  <Menu.Label className="px-2 pt-1 text-[11px] font-medium text-muted-foreground uppercase">
    {children}
  </Menu.Label>
);

/** Options shared by the Debug split button and the tool window settings. */
export const DebugOptionItems: FC<{ withLifecycle: boolean }> = ({ withLifecycle }) => {
  const { t } = useTranslation('debugger');
  const keymap = useDebugStore((s) => s.keymap);
  const inline = useDebugStore((s) => s.inlineValues);
  const options = useDebugStore((s) => s.startOptions);
  const patch = useDebugStore((s) => s.patchStartOptions);
  const setInline = useDebugStore((s) => s.setInlineValues);
  return (
    <>
      {withLifecycle && (
        <>
          <Label>{t('options.txMode')}</Label>
          <Menu.RadioGroup
            value={options.txMode}
            onValueChange={(v) => patch({ txMode: v as DebugTxMode })}
          >
            <RadioRow value="wrap">{t('options.txWrap')}</RadioRow>
            <RadioRow value="lease">{t('options.txLease')}</RadioRow>
          </Menu.RadioGroup>
          <Menu.Separator className={menuSeparatorClass} />
        </>
      )}
      <Label>{t('options.endAction')}</Label>
      <Menu.RadioGroup
        value={options.endAction}
        onValueChange={(v) => patch({ endAction: v as DebugEndAction })}
      >
        <RadioRow value="ask">{t('options.endAsk')}</RadioRow>
        <RadioRow value="rollback">{t('options.endRollback')}</RadioRow>
        <RadioRow value="commit">{t('options.endCommit')}</RadioRow>
        <RadioRow value="keep">{t('options.endKeep')}</RadioRow>
      </Menu.RadioGroup>
      <Menu.Separator className={menuSeparatorClass} />
      {withLifecycle && (
        <>
          <CheckRow
            checked={options.stopOnEntry !== false}
            onCheckedChange={(v) => patch({ stopOnEntry: v })}
          >
            {t('options.stopOnEntry')}
          </CheckRow>
          <CheckRow
            checked={options.breakOnError}
            onCheckedChange={(v) => patch({ breakOnError: v })}
          >
            {t('options.breakOnError')}
          </CheckRow>
        </>
      )}
      <CheckRow checked={inline} onCheckedChange={setInline}>
        {t('options.inlineValues')}
      </CheckRow>
      <Menu.Separator className={menuSeparatorClass} />
      <Label>{t('options.keymap')}</Label>
      <Menu.RadioGroup value={keymap} onValueChange={(v) => void saveKeymap(v as DebugKeymap)}>
        <RadioRow value="jetBrains">{t('options.keymapJetBrains')}</RadioRow>
        <RadioRow value="vsCode">{t('options.keymapVsCode')}</RadioRow>
      </Menu.RadioGroup>
    </>
  );
};

/** Gear menu of the tool window toolbar. */
export const SettingsMenu: FC<{ tabId: string; trigger: ReactNode; label: string }> = ({
  trigger,
  label,
}) => (
  <Menu.Root>
    <Tooltip content={label} side="bottom">
      <Menu.Trigger asChild>
        <Button size="icon" variant="ghost" aria-label={label}>
          {trigger}
        </Button>
      </Menu.Trigger>
    </Tooltip>
    <Menu.Portal>
      <Menu.Content side="top" align="start" sideOffset={4} className={menuContentClass}>
        <DebugOptionItems withLifecycle={false} />
      </Menu.Content>
    </Menu.Portal>
  </Menu.Root>
);
