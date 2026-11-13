import type { ConnectionProfile } from '@/lib/types';

export interface ConnectionDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Profile to edit; `undefined` creates a new connection. */
  profile?: ConnectionProfile;
}

/** Placeholder: the connection form is implemented by the connections frontend agent. */
export function ConnectionDialog({ open, profile }: ConnectionDialogProps) {
  if (!open) return null;
  return <div data-testid="connection-dialog">ConnectionDialog {profile?.name ?? '(new)'}</div>;
}
