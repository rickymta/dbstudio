import { ChevronRight } from 'lucide-react';
import { ContextMenu as Primitive } from 'radix-ui';
import type { ComponentProps } from 'react';
import { cn } from '@/lib/cn';
import { menuContentClass, menuItemClass, menuSeparatorClass } from './menuStyles';

export const ContextMenu = Primitive.Root;
export const ContextMenuTrigger = Primitive.Trigger;
export const ContextMenuSub = Primitive.Sub;

export function ContextMenuContent({
  className,
  ...rest
}: ComponentProps<typeof Primitive.Content>) {
  return (
    <Primitive.Portal>
      <Primitive.Content className={cn(menuContentClass, className)} {...rest} />
    </Primitive.Portal>
  );
}

export function ContextMenuItem({
  className,
  variant,
  ...rest
}: ComponentProps<typeof Primitive.Item> & { variant?: 'danger' }) {
  return (
    <Primitive.Item data-variant={variant} className={cn(menuItemClass, className)} {...rest} />
  );
}

export function ContextMenuSeparator({
  className,
  ...rest
}: ComponentProps<typeof Primitive.Separator>) {
  return <Primitive.Separator className={cn(menuSeparatorClass, className)} {...rest} />;
}

export function ContextMenuSubTrigger({
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

export function ContextMenuSubContent({
  className,
  ...rest
}: ComponentProps<typeof Primitive.SubContent>) {
  return (
    <Primitive.Portal>
      <Primitive.SubContent className={cn(menuContentClass, className)} {...rest} />
    </Primitive.Portal>
  );
}
