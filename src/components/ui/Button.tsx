import type { ButtonHTMLAttributes } from 'react';
import { cn } from '@/lib/cn';

type Variant = 'primary' | 'ghost' | 'outline' | 'danger';
type Size = 'sm' | 'md' | 'icon';

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  size?: Size;
}

const variants: Record<Variant, string> = {
  primary: 'bg-accent text-accent-foreground hover:opacity-90',
  ghost: 'hover:bg-surface-muted text-foreground',
  danger: 'bg-danger text-accent-foreground hover:opacity-90',
  outline: 'border border-border bg-surface hover:bg-surface-muted text-foreground',
};

const sizes: Record<Size, string> = {
  sm: 'control-small h-7 px-2 text-xs gap-1',
  md: 'control h-8 px-3 text-[13px] gap-1.5',
  icon: 'control-small h-7 min-w-7 shrink-0',
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
