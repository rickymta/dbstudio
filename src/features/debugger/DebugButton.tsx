import { Bug, ChevronDown, Info } from 'lucide-react';
import { DropdownMenu as Menu } from 'radix-ui';
import { useEffect, useState, type FC } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Spinner } from '@/components/ui/Spinner';
import { menuContentClass, menuItemClass, menuSeparatorClass } from '@/components/ui/menuStyles';
import type { EditorMode } from '@/stores/tabs';
import {
  ensureDebugEvents,
  loadKeymap,
  loadSupport,
  onSupportChanged,
  runCommand,
  supportOf,
} from './debugActions';
import { isLive, useDebugStore } from './debugStore';
import { useDebugUi } from './debugUiStore';
import { ariaShortcut, shortcutLabel } from './keymap';
import { DebugOptionItems } from './SettingsMenu';

export interface DebugButtonProps {
  tabStateId: string;
  sessionId: string | null;
  connectionId: string | null;
  mode: EditorMode;
}

let keymapLoaded = false;

/** The "Debug" split button of the editor toolbar: start + a menu with the session options. */
export const DebugButton: FC<DebugButtonProps> = ({ tabStateId, sessionId, mode }) => {
  const { t } = useTranslation('debugger');
  const live = useDebugStore((s) => isLive(s.sessions[tabStateId]));
  const starting = useDebugStore((s) => !!s.starting[tabStateId]);
  const keymap = useDebugStore((s) => s.keymap);
  const [, bump] = useState(0);

  useEffect(() => {
    void ensureDebugEvents();
    if (!keymapLoaded) {
      keymapLoaded = true;
      void loadKeymap();
    }
    return onSupportChanged(() => bump((n) => n + 1));
  }, []);

  if (mode === 'redis') return null;
  const support = supportOf(tabStateId);
  const keys = shortcutLabel('debug', keymap);
  const title = keys ? `${t('button.debug')} (${keys})` : t('button.debug');
  const disabled = !sessionId || live || starting;

  return (
    <span className="inline-flex" role="group" aria-label={t('button.group')}>
      <Button
        size="sm"
        variant="outline"
        className="rounded-r-none"
        disabled={disabled}
        title={title}
        aria-keyshortcuts={ariaShortcut('debug', keymap)}
        onClick={() => runCommand(tabStateId, 'debug')}
      >
        {starting ? <Spinner size={13} /> : <Bug size={14} aria-hidden />} {t('button.debug')}
      </Button>
      <Menu.Root
        onOpenChange={(open) => {
          if (open && sessionId && !support) void loadSupport(tabStateId, sessionId);
        }}
      >
        <Menu.Trigger asChild>
          <Button
            size="sm"
            variant="outline"
            className="-ml-px w-6 rounded-l-none px-0"
            aria-label={t('button.options')}
            disabled={!sessionId}
          >
            <ChevronDown size={13} aria-hidden />
          </Button>
        </Menu.Trigger>
        <Menu.Portal>
          <Menu.Content align="start" sideOffset={4} className={menuContentClass}>
            <DebugOptionItems withLifecycle />
            <Menu.Separator className={menuSeparatorClass} />
            <Menu.Item
              className={menuItemClass}
              disabled={!support}
              onSelect={() =>
                useDebugUi.getState().openDialog(tabStateId, { kind: 'routineSupport' })
              }
            >
              <Info size={13} aria-hidden />
              {support ? t('options.routineStatus') : t('options.routineProbe')}
            </Menu.Item>
            <Menu.Item
              className={menuItemClass}
              onSelect={() => useDebugUi.getState().openDialog(tabStateId, { kind: 'breakpoints' })}
            >
              {t('command.viewBreakpoints')}
            </Menu.Item>
          </Menu.Content>
        </Menu.Portal>
      </Menu.Root>
    </span>
  );
};
