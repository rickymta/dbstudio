import type { ReactNode } from 'react';
import type { ExportOrigin } from './open';

export interface ExportWizardProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  origin: ExportOrigin;
}

/** Export wizard dialog (placeholder: renders nothing). */
export function ExportWizard({ open, onOpenChange, origin }: ExportWizardProps): ReactNode {
  void [open, onOpenChange, origin];
  return null;
}
