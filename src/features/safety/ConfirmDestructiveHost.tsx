import type { ReactNode } from 'react';
import { CONFIRMATION_DECLINED, useConfirmStore } from './confirmStore';
import { ConfirmDestructiveDialog } from './ConfirmDestructiveDialog';

/** Renders the pending destructive confirmation (mounted once, in the status bar). */
export function ConfirmDestructiveHost(): ReactNode {
  const head = useConfirmStore((s) => s.queue[0]);
  const shift = useConfirmStore((s) => s.shift);
  return (
    <ConfirmDestructiveDialog
      request={head?.request ?? null}
      onConfirm={(grant) => {
        head?.resolve(grant);
        shift();
      }}
      onCancel={() => {
        head?.reject(CONFIRMATION_DECLINED);
        shift();
      }}
    />
  );
}
