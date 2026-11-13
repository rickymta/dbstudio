import type { ReactNode } from 'react';
import type { ObjectPath, SessionInfo } from '@/lib/types';

export interface ImportWizardProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  session: SessionInfo;
  /** Table to import into; omitted = new table or SQL script. */
  target?: ObjectPath;
}

/** Import wizard dialog (placeholder: renders nothing). */
export function ImportWizard({
  open,
  onOpenChange,
  session,
  target,
}: ImportWizardProps): ReactNode {
  void [open, onOpenChange, session, target];
  return null;
}
