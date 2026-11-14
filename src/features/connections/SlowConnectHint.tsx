import { useTranslation } from 'react-i18next';
import { useSlowAfter } from './useSlowAfter';

/**
 * Note shown next to a connect/test/save spinner that has been running for a while. A stored
 * password is read through the OS keychain, which may be waiting on a system dialog.
 */
export function SlowConnectHint({
  active,
  keychain,
  className,
}: {
  active: boolean;
  /** The profile reads or writes a password in the OS secret store. */
  keychain: boolean;
  className?: string;
}) {
  const { t } = useTranslation('connections');
  const slow = useSlowAfter(active);
  if (!active || !slow) return null;
  return (
    <p role="status" className={className ?? 'text-xs text-muted-foreground'}>
      {t(keychain ? 'slow.keychain' : 'slow.generic')}
    </p>
  );
}
