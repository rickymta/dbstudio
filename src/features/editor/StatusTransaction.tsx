import { useTranslation } from 'react-i18next';
import { useTabsStore } from '@/stores/tabs';
import { useTabData } from './editorStore';

/** Status-bar cell: transaction state of the active editor tab (APP-03). Hidden for other tabs. */
export function StatusTransaction() {
  const { t } = useTranslation('editor');
  const tabStateId = useTabsStore((s) => {
    const tab = s.tabs.find((x) => x.id === s.activeId);
    return tab?.kind === 'editor' ? tab.payload.tabStateId : '';
  });
  const data = useTabData(tabStateId);
  if (!data) return null;
  const state = data.leaseState;
  const autoCommit = state?.autoCommit ?? data.autoCommit;
  const tx = state?.transaction ?? 'idle';
  const label =
    tx === 'idle'
      ? autoCommit
        ? t('tx.status.idleAuto')
        : t('tx.status.idleManual')
      : t(`tx.status.${tx}`);
  const hot = tx !== 'idle';
  return (
    <span
      data-testid="status-transaction"
      data-tx={tx}
      className="flex items-center gap-1.5 border-l border-border pl-3"
    >
      <span
        aria-hidden
        className={`size-2 shrink-0 rounded-full ${hot ? 'bg-warning' : 'bg-border'}`}
      />
      <span className={hot ? 'font-medium text-warning' : undefined}>{label}</span>
    </span>
  );
}
