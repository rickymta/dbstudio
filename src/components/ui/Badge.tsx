import type { HTMLAttributes } from 'react';
import { cn } from '@/lib/cn';

type Tone = 'neutral' | 'success' | 'warning';

const tones: Record<Tone, string> = {
  neutral: 'bg-surface-muted text-muted-foreground',
  success: 'bg-success-soft text-success',
  warning: 'bg-warning-soft text-warning',
};

export function Badge({
  tone = 'neutral',
  className,
  ...rest
}: HTMLAttributes<HTMLSpanElement> & { tone?: Tone }) {
  return (
    <span
      className={cn(
        'inline-flex items-center rounded px-1.5 py-0.5 text-[11px] font-medium leading-none',
        tones[tone],
        className,
      )}
      {...rest}
    />
  );
}
