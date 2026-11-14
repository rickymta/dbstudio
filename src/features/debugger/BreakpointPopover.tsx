import { useEffect, useRef, type FC } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { removeBreakpoint } from './debugActions';
import { useDebugStore, type ScopeInfo } from './debugStore';
import { useDebugUi } from './debugUiStore';
import { BreakpointForm } from './BreakpointForm';

export interface BreakpointPopoverProps {
  /** Which scopes this host shows the popover for. */
  match: (scope: ScopeInfo) => boolean;
  /** Tab whose Breakpoints dialog "More" opens (none: no such button). */
  moreTabId: string | null;
}

/** JetBrains style popover of a gutter breakpoint (right click). */
export const BreakpointPopover: FC<BreakpointPopoverProps> = ({ match, moreTabId }) => {
  const { t } = useTranslation('debugger');
  const popover = useDebugUi((s) => s.popover);
  const scope = useDebugStore((s) => (popover ? s.scopes[popover.scopeKey] : undefined));
  const ref = useRef<HTMLDivElement>(null);
  const shown = !!popover && !!scope && match(scope);

  useEffect(() => {
    if (!shown) return;
    const onDown = (e: PointerEvent) => {
      if (!ref.current?.contains(e.target as Node)) useDebugUi.getState().closePopover();
    };
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key === 'Escape') useDebugUi.getState().closePopover();
    };
    document.addEventListener('pointerdown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('pointerdown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [shown]);

  const breakpoint = scope?.breakpoints.find((b) => b.id === popover?.breakpointId);
  if (!shown || !popover || !breakpoint) return null;
  const left = Math.max(8, Math.min(popover.x, window.innerWidth - 300));
  const top = Math.max(8, Math.min(popover.y, window.innerHeight - 340));
  return (
    <div
      ref={ref}
      role="dialog"
      aria-label={t('breakpoint.popoverTitle', { line: breakpoint.line })}
      className="fixed z-50 w-72 rounded-lg border border-border bg-surface p-3 text-foreground shadow-xl"
      style={{ left, top }}
    >
      <div className="mb-2 text-xs font-medium">
        {t('breakpoint.popoverTitle', { line: breakpoint.line })}
      </div>
      <BreakpointForm scopeKey={popover.scopeKey} breakpoint={breakpoint} />
      <div className="mt-3 flex justify-between">
        <Button
          size="sm"
          variant="outline"
          onClick={() => {
            removeBreakpoint(popover.scopeKey, breakpoint.id);
            useDebugUi.getState().closePopover();
          }}
        >
          {t('breakpoint.remove')}
        </Button>
        {moreTabId && (
          <Button
            size="sm"
            onClick={() => {
              useDebugUi.getState().closePopover();
              useDebugUi.getState().openDialog(moreTabId, { kind: 'breakpoints' });
            }}
          >
            {t('breakpoint.more')}
          </Button>
        )}
      </div>
    </div>
  );
};
