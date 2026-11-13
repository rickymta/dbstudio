import type { ButtonHTMLAttributes } from 'react';
import { cn } from '@/lib/cn';

type Variant = 'primary' | 'ghost' | 'outline';
type Size = 'sm' | 'md' | 'icon';

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  size?: Size;
}

const variants: Record<Variant, string> = {
  primary: 'bg-accent text-accent-foreground hover:opacity-90',
  ghost: 'hover:bg-surface-muted text-foreground',
  outline: 'border border-border bg-surface hover:bg-surface-muted text-foreground',
};

const sizes: Record<Size, string> = {
  sm: 'h-6 px-2 text-xs gap-1',
  md: 'h-7 px-3 text-[13px] gap-1.5',
  icon: 'h-6 w-6',
};

export function Button({
  variant = 'outline',
  size = 'md',
  className,
  type = 'button',
  ...rest
}: ButtonProps) {
  return (
    <button
      type={type}
      className={cn(
        'inline-flex items-center justify-center rounded-md font-medium transition-colors',
        'disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent',
        variants[variant],
        sizes[size],
        className,
      )}
      {...rest}
    />
  );
}
