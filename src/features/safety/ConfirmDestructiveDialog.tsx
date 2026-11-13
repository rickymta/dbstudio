import type { ReactNode } from 'react';
import type { ConfirmationGrant, ConfirmationRequest } from '@/lib/types';

export interface ConfirmDestructiveDialogProps {
  /** The `confirmation` of a `confirmationRequired` error; `null` closes the dialog. */
  request: ConfirmationRequest | null;
  /** The user typed `request.confirmText`: call the command again with this grant. */
  onConfirm: (grant: ConfirmationGrant) => void;
  onCancel: () => void;
}

/** Asks the user to type the confirmation text before a destructive statement runs. */
export function ConfirmDestructiveDialog({
  request,
  onConfirm,
  onCancel,
}: ConfirmDestructiveDialogProps): ReactNode {
  void [request, onConfirm, onCancel];
  return null;
}
