import { BACKUP_TAB_ID, useTabsStore } from '@/stores/tabs';

/** Open (or focus) the singleton backup tab. */
export function openBackup(): void {
  useTabsStore.getState().openTab({
    id: BACKUP_TAB_ID,
    kind: 'backup',
    title: { key: 'tab.title', ns: 'backup' },
    closable: true,
  });
}
