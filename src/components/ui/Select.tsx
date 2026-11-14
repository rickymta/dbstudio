import { Check, ChevronDown } from 'lucide-react';
import { Select as SelectPrimitive } from 'radix-ui';
import { cn } from '@/lib/cn';

export interface SelectOption {
  /** Must not be an empty string (Radix restriction). */
  value: string;
  label: string;
  disabled?: boolean;
}

interface SelectProps {
  value?: string;
  onValueChange: (value: string) => void;
  options: SelectOption[];
  placeholder?: string;
  disabled?: boolean;
  id?: string;
  'aria-label'?: string;
  className?: string;
}

/** Options-driven select with a compact trigger. */
export function Select({
  value,
  onValueChange,
  options,
  placeholder,
  disabled,
  id,
  className,
  ...aria
}: SelectProps) {
  return (
    <SelectPrimitive.Root value={value} onValueChange={onValueChange} disabled={disabled}>
      <SelectPrimitive.Trigger
        id={id}
        aria-label={aria['aria-label']}
        className={cn(
          'inline-flex control h-8 w-full items-center justify-between gap-2 rounded-md border border-border',
          'bg-surface px-2 text-[13px] text-foreground data-[placeholder]:text-muted-foreground',
          'disabled:cursor-not-allowed disabled:opacity-50',
          className,
        )}
      >
        <SelectPrimitive.Value placeholder={placeholder} />
        <SelectPrimitive.Icon>
          <ChevronDown size={14} className="text-muted-foreground" />
        </SelectPrimitive.Icon>
      </SelectPrimitive.Trigger>
      <SelectPrimitive.Portal>
        <SelectPrimitive.Content
          position="popper"
          sideOffset={4}
          className="z-50 max-h-72 min-w-[var(--radix-select-trigger-width)] overflow-hidden rounded-md border border-border bg-surface text-foreground shadow-lg"
        >
          <SelectPrimitive.Viewport className="p-1">
            {options.map((o) => (
              <SelectPrimitive.Item
                key={o.value}
                value={o.value}
                disabled={o.disabled}
                className="relative flex h-7 cursor-default items-center rounded px-6 text-[13px] outline-none data-[disabled]:opacity-50 data-[highlighted]:bg-surface-muted"
              >
                <SelectPrimitive.ItemIndicator className="absolute left-1.5">
                  <Check size={12} />
                </SelectPrimitive.ItemIndicator>
                <SelectPrimitive.ItemText>{o.label}</SelectPrimitive.ItemText>
              </SelectPrimitive.Item>
            ))}
          </SelectPrimitive.Viewport>
        </SelectPrimitive.Content>
      </SelectPrimitive.Portal>
    </SelectPrimitive.Root>
  );
}
