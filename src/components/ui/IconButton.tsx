import type { ReactNode } from 'react';
import { Button, type ButtonProps } from './Button';
import { Tooltip } from './Tooltip';

interface IconButtonProps extends Omit<ButtonProps, 'size' | 'children' | 'aria-label'> {
  /** Accessible name; also shown as a tooltip. */
  label: string;
  icon: ReactNode;
}

export function IconButton({ label, icon, variant = 'ghost', ...rest }: IconButtonProps) {
  return (
    <Tooltip content={label}>
      <Button size="icon" variant={variant} aria-label={label} {...rest}>
        {icon}
      </Button>
    </Tooltip>
  );
}
