import { useId, type ReactElement, cloneElement } from 'react';
import { cn } from '@/lib/cn';

interface TooltipProps {
  content: string;
  children: ReactElement<{ 'aria-describedby'?: string }>;
  side?: 'top' | 'bottom';
  className?: string;
}

/**
 * Lightweight CSS tooltip. The wrapper (not the child) receives hover, so it also works on
 * disabled buttons. The bubble is `display: none` until shown (it still describes the child for
 * assistive technology): an invisible but laid-out bubble widens the scrollable area of every
 * ancestor, e.g. the "New query" bubble made the tab bar overflow its pane by 8px.
 */
export function Tooltip({ content, children, side = 'top', className }: TooltipProps) {
  const id = useId();
  return (
    <span className={cn('group/tooltip relative inline-flex', className)}>
      {cloneElement(children, { 'aria-describedby': id })}
      <span
        id={id}
        role="tooltip"
        className={cn(
          'pointer-events-none absolute left-1/2 z-50 -translate-x-1/2 whitespace-nowrap rounded',
          'hidden bg-foreground px-2 py-1 text-xs text-background shadow-md',
          'group-hover/tooltip:block group-focus-within/tooltip:block',
          side === 'top' ? 'bottom-full mb-1.5' : 'top-full mt-1.5',
        )}
      >
        {content}
      </span>
    </span>
  );
}
