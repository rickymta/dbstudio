import type { InputHTMLAttributes } from 'react';
import { cn } from '@/lib/cn';

export const fieldClass =
  'w-full select-text rounded-md border border-border bg-surface px-2 text-[13px] text-foreground ' +
  'placeholder:text-muted-foreground disabled:cursor-not-allowed disabled:opacity-50 ' +
  'aria-[invalid=true]:border-danger';

export function Input({
  className,
  type = 'text',
  ...rest
}: InputHTMLAttributes<HTMLInputElement>) {
  return <input type={type} className={cn(fieldClass, 'h-7', className)} {...rest} />;
}
