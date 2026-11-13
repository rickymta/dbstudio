import { Tabs as Primitive } from 'radix-ui';
import type { ComponentProps } from 'react';
import { cn } from '@/lib/cn';

/** Sub-tabs inside a panel (distinct from the application tab bar). */
export const Tabs = Primitive.Root;

export function TabsList({ className, ...rest }: ComponentProps<typeof Primitive.List>) {
  return (
    <Primitive.List
      className={cn('flex h-8 shrink-0 items-end gap-3 border-b border-border px-2', className)}
      {...rest}
    />
  );
}

export function TabsTrigger({ className, ...rest }: ComponentProps<typeof Primitive.Trigger>) {
  return (
    <Primitive.Trigger
      className={cn(
        '-mb-px h-8 border-b-2 border-transparent px-1 text-[13px] text-muted-foreground',
        'hover:text-foreground data-[state=active]:border-accent data-[state=active]:text-foreground',
        className,
      )}
      {...rest}
    />
  );
}

export function TabsContent({ className, ...rest }: ComponentProps<typeof Primitive.Content>) {
  return <Primitive.Content className={cn('min-h-0 flex-1 outline-none', className)} {...rest} />;
}
