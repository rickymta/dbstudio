import type { TextareaHTMLAttributes } from 'react';
import { cn } from '@/lib/cn';
import { fieldClass } from './Input';

export function Textarea({
  className,
  rows = 3,
  ...rest
}: TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return (
    <textarea rows={rows} className={cn(fieldClass, 'py-1.5 font-mono', className)} {...rest} />
  );
}
