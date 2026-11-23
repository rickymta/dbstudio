// The two toolbars of the Debug tool window: the vertical one on the left (session control) and
// the horizontal step bar (design 5.1).

import {
  ArrowDownToLine,
  ArrowDownToDot,
  ArrowUpFromLine,
  Calculator,
  CircleDot,
  Crosshair,
  MousePointerClick,
  Pause,
  Play,
  RefreshCw,
  Settings,
  Square,
  StepForward,
  Target,
  BellOff,
  type LucideIcon,
} from 'lucide-react';
import { useRef, type FC, type KeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Tooltip } from '@/components/ui/Tooltip';
import { cn } from '@/lib/cn';
import { commandEnabled, runCommand } from './debugActions';
import { useDebugStore } from './debugStore';
import { ariaShortcut, shortcutLabel, type DebugCommand } from './keymap';
import { SettingsMenu } from './SettingsMenu';

interface ToolButtonProps {
  tabId: string;
  command: DebugCommand;
  icon: LucideIcon;
  /** Also disabled by the caller (e.g. no execution line). */
  disabled?: boolean;
  pressed?: boolean;
  tone?: 'accent' | 'danger' | 'success';
  scopeKey?: string;
}

/** Icon button of a debugger command: label + shortcut in tooltip and accessible name. */
export const ToolButton: FC<ToolButtonProps> = ({
  tabId,
  command,
  icon: Icon,
  disabled,
  pressed,
  tone,
  scopeKey,
}) => {
  const { t } = useTranslation('debugger');
  const keymap = useDebugStore((s) => s.keymap);
  // Subscribe to what `commandEnabled` reads so the button follows the session.
  useDebugStore((s) => s.sessions[tabId]);
  const keys = shortcutLabel(command, keymap);
  const label = t(`command.${command}`);
  const text = keys ? `${label} (${keys})` : label;
  const enabled = commandEnabled(tabId, command) && !disabled;
  return (
    <Tooltip content={text}>
      <Button
        size="icon"
        variant="ghost"
        aria-label={text}
        aria-keyshortcuts={ariaShortcut(command, keymap)}
        aria-pressed={pressed}
        disabled={!enabled}
        data-command={command}
        className={cn(
          pressed && 'bg-surface-muted',
          tone === 'accent' && 'text-accent',
          tone === 'danger' && 'text-danger',
          tone === 'success' && 'text-success',
        )}
        onClick={() => runCommand(tabId, command, { scopeKey })}
      >
        <Icon size={15} aria-hidden />
      </Button>
    </Tooltip>
  );
};

/** Arrow-key navigation between the enabled buttons of a toolbar. */
function useToolbarKeys(orientation: 'horizontal' | 'vertical') {
  const ref = useRef<HTMLDivElement>(null);
  const [prev, next] =
    orientation === 'vertical' ? ['ArrowUp', 'ArrowDown'] : ['ArrowLeft', 'ArrowRight'];
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key !== prev && e.key !== next && e.key !== 'Home' && e.key !== 'End') return;
    const buttons = [
      ...(ref.current?.querySelectorAll<HTMLButtonElement>('button:not(:disabled)') ?? []),
    ];
    if (buttons.length === 0) return;
    const at = buttons.indexOf(document.activeElement as HTMLButtonElement);
    const to =
      e.key === 'Home'
        ? 0
        : e.key === 'End'
          ? buttons.length - 1
          : (at + (e.key === next ? 1 : -1) + buttons.length) % buttons.length;
    e.preventDefault();
    buttons[to]?.focus();
  };
  return { ref, onKeyDown };
}

/** Rerun, Resume, Pause, Stop, View Breakpoints, Mute, Settings (JetBrains classic). */
export const SessionToolbar: FC<{ tabId: string }> = ({ tabId }) => {
  const { t } = useTranslation('debugger');
  const muted = useDebugStore((s) => s.muted[tabId] ?? false);
  const nav = useToolbarKeys('vertical');
  return (
    <div
      {...nav}
      role="toolbar"
      aria-orientation="vertical"
      aria-label={t('toolbar.session')}
      className="flex w-9 shrink-0 flex-col items-center gap-0.5 border-r border-border bg-surface-muted/50 py-1"
    >
      <ToolButton tabId={tabId} command="rerun" icon={RefreshCw} />
      <ToolButton tabId={tabId} command="resume" icon={Play} tone="success" />
      <ToolButton tabId={tabId} command="pause" icon={Pause} />
      <ToolButton tabId={tabId} command="stop" icon={Square} tone="danger" />
      <span className="my-0.5 h-px w-5 bg-border" aria-hidden />
      <ToolButton tabId={tabId} command="viewBreakpoints" icon={CircleDot} />
      <ToolButton tabId={tabId} command="muteBreakpoints" icon={BellOff} pressed={muted} />
      <span className="flex-1" />
      <SettingsMenu
        tabId={tabId}
        trigger={<Settings size={15} aria-hidden />}
        label={t('toolbar.settings')}
      />
    </div>
  );
};

/** Show Execution Point, Step Over/Into/Force/Out, Run to Cursor, Evaluate. */
export const StepBar: FC<{ tabId: string; scopeKey?: string }> = ({ tabId, scopeKey }) => {
  const { t } = useTranslation('debugger');
  const hasTargets = useDebugStore(
    (s) => (s.sessions[tabId]?.state.stepIntoTargets?.length ?? 0) > 0,
  );
  const nav = useToolbarKeys('horizontal');
  return (
    <div
      {...nav}
      role="toolbar"
      aria-orientation="horizontal"
      aria-label={t('toolbar.steps')}
      className="flex items-center gap-0.5"
    >
      <ToolButton tabId={tabId} command="showExecutionPoint" icon={Crosshair} scopeKey={scopeKey} />
      <span className="mx-1 h-4 w-px bg-border" aria-hidden />
      <ToolButton tabId={tabId} command="stepOver" icon={StepForward} scopeKey={scopeKey} />
      <ToolButton tabId={tabId} command="stepInto" icon={ArrowDownToLine} scopeKey={scopeKey} />
      {hasTargets && (
        <ToolButton
          tabId={tabId}
          command="smartStepInto"
          icon={MousePointerClick}
          scopeKey={scopeKey}
        />
      )}
      <ToolButton tabId={tabId} command="forceStepInto" icon={ArrowDownToDot} scopeKey={scopeKey} />
      <ToolButton tabId={tabId} command="stepOut" icon={ArrowUpFromLine} scopeKey={scopeKey} />
      <ToolButton tabId={tabId} command="runToCursor" icon={Target} scopeKey={scopeKey} />
      <span className="mx-1 h-4 w-px bg-border" aria-hidden />
      <ToolButton tabId={tabId} command="evaluate" icon={Calculator} scopeKey={scopeKey} />
    </div>
  );
};
