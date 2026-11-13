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
 * disabled buttons.
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
          'bg-foreground px-2 py-1 text-xs text-background opacity-0 shadow-md transition-opacity',
          'group-hover/tooltip:opacity-100 group-focus-within/tooltip:opacity-100',
          side === 'top' ? 'bottom-full mb-1.5' : 'top-full mt-1.5',
        )}
      >
        {content}
      </span>
    </span>
  );
}
