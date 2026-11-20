// Closing a tab with pending edits asks first. The guard is registered from the pending store, not
// from a component: only the active tab is mounted, but a background tab can be closed too.

import { confirmDialog } from '@/components/ui/confirm';
import i18n from '@/lib/i18n';
import { registerTabGuard } from '@/stores/tabGuard';
import { countChanges, usePendingChanges } from './pendingChanges';

const unregister = new Map<string, () => void>();

function confirmClose(): Promise<boolean> {
  return confirmDialog({
    title: i18n.t('confirmDialog.unsavedTitle'),
    description: i18n.t('data:edit.closeConfirm'),
    confirmLabel: i18n.t('actions.close'),
    destructive: true,
  });
}

usePendingChanges.subscribe((state) => {
  for (const [tabId, p] of Object.entries(state.byTab)) {
    if (countChanges(p) > 0 && !unregister.has(tabId)) {
      unregister.set(tabId, registerTabGuard(tabId, confirmClose));
    }
  }
  for (const [tabId, off] of [...unregister]) {
    const p = state.byTab[tabId];
    if (!p || countChanges(p) === 0) {
      off();
      unregister.delete(tabId);
    }
  }
});
