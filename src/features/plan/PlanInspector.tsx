import { Lightbulb, PanelRightClose } from 'lucide-react';
import { useRef, type KeyboardEvent, type PointerEvent, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';
import { INSPECTOR_MAX, INSPECTOR_MIN } from './planPrefs';

interface PlanInspectorProps {
  collapsed: boolean;
  onCollapsedChange: (collapsed: boolean) => void;
  width: number;
  /** `persist` is false while dragging and true for the final value. */
  onWidthChange: (width: number, persist: boolean) => void;
  /** Number shown next to the icon of the collapsed strip. */
  badge?: number;
  children: ReactNode;
}

/**
 * Right-hand panel of the plan tab: a resizable (drag handle or arrow keys), collapsible shell
 * around the inspector content. Collapsed, it is a 28px strip with an icon and a count.
 */
export function PlanInspector({
  collapsed,
  onCollapsedChange,
  width,
  onWidthChange,
  badge = 0,
  children,
}: PlanInspectorProps) {
  const { t } = useTranslation('plan');
  const drag = useRef<{ x: number; width: number } | null>(null);

  if (collapsed) {
    return (
      <button
        type="button"
        data-testid="plan-inspector-collapsed"
        aria-label={t('inspector.expand')}
        title={t('inspector.expand')}
        onClick={() => onCollapsedChange(false)}
        className="flex h-full w-7 shrink-0 flex-col items-center gap-1 border-l border-border bg-surface pt-2 text-muted-foreground hover:bg-surface-muted"
      >
        <Lightbulb size={14} aria-hidden />
        {badge > 0 && <span className="text-[10px] tabular-nums">{badge}</span>}
      </button>
    );
  }

  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    drag.current = { x: e.clientX, width };
    e.currentTarget.setPointerCapture(e.pointerId);
  };
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (d) onWidthChange(d.width + (d.x - e.clientX), false);
  };
  const onPointerUp = (e: PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    drag.current = null;
    if (d) onWidthChange(d.width + (d.x - e.clientX), true);
  };
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const step = e.shiftKey ? 40 : 10;
    if (e.key === 'ArrowLeft') onWidthChange(width + step, true);
    else if (e.key === 'ArrowRight') onWidthChange(width - step, true);
    else return;
    e.preventDefault();
  };

  return (
    <div className="relative flex h-full shrink-0" style={{ width }} data-testid="plan-inspector">
      <div
        role="separator"
        aria-orientation="vertical"
        aria-label={t('inspector.resize')}
        aria-valuemin={INSPECTOR_MIN}
        aria-valuemax={INSPECTOR_MAX}
        aria-valuenow={width}
        tabIndex={0}
        data-testid="plan-inspector-handle"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onKeyDown={onKeyDown}
        className={cn(
          'absolute inset-y-0 -left-1 z-20 w-2 cursor-col-resize touch-none',
          'hover:bg-accent/30 focus-visible:bg-accent/40 focus-visible:outline-none',
        )}
      />
      <div className="flex min-w-0 flex-1 flex-col border-l border-border bg-surface">
        <div className="flex h-6 shrink-0 items-center justify-end border-b border-border px-1">
          <button
            type="button"
            aria-label={t('inspector.collapse')}
            title={t('inspector.collapse')}
            onClick={() => onCollapsedChange(true)}
            className="rounded p-0.5 text-muted-foreground hover:bg-surface-muted"
          >
            <PanelRightClose size={14} aria-hidden />
          </button>
        </div>
        {/* PlanDetails brings its own fixed width and left border: the shell decides both. */}
        <div className="min-h-0 flex-1 [&>aside]:h-full [&>aside]:w-full [&>aside]:border-l-0">
          {children}
        </div>
      </div>
    </div>
  );
}
