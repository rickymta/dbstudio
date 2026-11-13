import { ScrollArea as Primitive } from 'radix-ui';
import type { ComponentProps } from 'react';
import { cn } from '@/lib/cn';

/** Scroll container with thin themed scrollbars. Give it a bounded height. */
export function ScrollArea({
  className,
  children,
  ...rest
}: ComponentProps<typeof Primitive.Root>) {
  return (
    <Primitive.Root className={cn('overflow-hidden', className)} {...rest}>
      <Primitive.Viewport className="size-full">{children}</Primitive.Viewport>
      {(['vertical', 'horizontal'] as const).map((orientation) => (
        <Primitive.Scrollbar
          key={orientation}
          orientation={orientation}
          className={cn(
            'flex touch-none select-none p-px',
            orientation === 'vertical' ? 'h-full w-2' : 'h-2 flex-col',
          )}
        >
          <Primitive.Thumb className="relative flex-1 rounded-full bg-border" />
        </Primitive.Scrollbar>
      ))}
      <Primitive.Corner />
    </Primitive.Root>
  );
}
