import type { ReactNode } from 'react';
import { cn } from '@/lib/cn';

interface TooltipProps {
  content: string;
  children: ReactNode;
  className?: string;
}

/**
 * Hint of a control: the native `title`, like every other hint in the app (and in GitStudio).
 * The system draws it above the window and keeps it on screen by itself, so it is never clipped
 * by a pane and never widens the page: the former CSS bubble of the last status bar button
 * stuck out of the window and added a horizontal scrollbar. The wrapper (not the child) carries
 * the title, so it also shows on a disabled button.
 */
export function Tooltip({ content, children, className }: TooltipProps) {
  return (
    <span title={content} className={cn('relative inline-flex', className)}>
      {children}
    </span>
  );
}
