import { RadioGroup as RadioGroupPrimitive } from 'radix-ui';
import type { ComponentProps } from 'react';
import { cn } from '@/lib/cn';

/** Group of [`Radio`] items (keyboard: arrow keys move the choice, like native radios). */
export function RadioGroup({
  className,
  ...rest
}: ComponentProps<typeof RadioGroupPrimitive.Root>) {
  return <RadioGroupPrimitive.Root className={cn('flex flex-col gap-1.5', className)} {...rest} />;
}

/** One choice of a [`RadioGroup`], styled like [`Checkbox`] (accent ring and dot when chosen). */
export function Radio({ className, ...rest }: ComponentProps<typeof RadioGroupPrimitive.Item>) {
  return (
    <RadioGroupPrimitive.Item
      className={cn(
        'flex size-4 shrink-0 items-center justify-center rounded-full border border-border bg-surface',
        'outline-none focus-visible:ring-2 focus-visible:ring-accent/50',
        'data-[state=checked]:border-accent',
        'disabled:cursor-not-allowed disabled:opacity-50',
        className,
      )}
      {...rest}
    >
      <RadioGroupPrimitive.Indicator className="size-2 rounded-full bg-accent" />
    </RadioGroupPrimitive.Item>
  );
}
