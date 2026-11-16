import { Check, ChevronRight } from 'lucide-react';
import { DropdownMenu as Primitive } from 'radix-ui';
import type { ComponentProps } from 'react';
import { cn } from '@/lib/cn';
import { menuContentClass, menuItemClass, menuSeparatorClass } from './menuStyles';

export const DropdownMenu = Primitive.Root;
export const DropdownMenuTrigger = Primitive.Trigger;
export const DropdownMenuSub = Primitive.Sub;
export const DropdownMenuRadioGroup = Primitive.RadioGroup;

export function DropdownMenuLabel({ className, ...rest }: ComponentProps<typeof Primitive.Label>) {
  return (
    <Primitive.Label
      className={cn('px-2 py-1 text-xs font-medium text-muted-foreground', className)}
      {...rest}
    />
  );
}

/** Item of a radio group: `role="menuitemradio"`, with a check mark when selected. */
export function DropdownMenuRadioItem({
  className,
  children,
  ...rest
}: ComponentProps<typeof Primitive.RadioItem>) {
  return (
    <Primitive.RadioItem className={cn(menuItemClass, 'min-w-0', className)} {...rest}>
      {children}
      <Primitive.ItemIndicator className="ml-auto shrink-0">
        <Check size={13} aria-hidden />
      </Primitive.ItemIndicator>
    </Primitive.RadioItem>
  );
}

export function DropdownMenuContent({
  className,
  ...rest
}: ComponentProps<typeof Primitive.Content>) {
  return (
    <Primitive.Portal>
      <Primitive.Content sideOffset={4} className={cn(menuContentClass, className)} {...rest} />
    </Primitive.Portal>
  );
}

export function DropdownMenuItem({
  className,
  variant,
  ...rest
}: ComponentProps<typeof Primitive.Item> & { variant?: 'danger' }) {
  return (
    <Primitive.Item data-variant={variant} className={cn(menuItemClass, className)} {...rest} />
  );
}

export function DropdownMenuSeparator({
  className,
  ...rest
}: ComponentProps<typeof Primitive.Separator>) {
  return <Primitive.Separator className={cn(menuSeparatorClass, className)} {...rest} />;
}

export function DropdownMenuSubTrigger({
  className,
  children,
  ...rest
}: ComponentProps<typeof Primitive.SubTrigger>) {
  return (
    <Primitive.SubTrigger className={cn(menuItemClass, 'justify-between', className)} {...rest}>
      {children}
      <ChevronRight size={12} />
    </Primitive.SubTrigger>
  );
}

export function DropdownMenuSubContent({
  className,
  ...rest
}: ComponentProps<typeof Primitive.SubContent>) {
  return (
    <Primitive.Portal>
      <Primitive.SubContent className={cn(menuContentClass, className)} {...rest} />
    </Primitive.Portal>
  );
}
