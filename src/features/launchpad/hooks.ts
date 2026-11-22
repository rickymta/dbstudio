import { useMemo } from 'react';
import { usePendingChanges } from '@/features/data/edit/pendingChanges';
import { useEditorStore } from '@/features/editor/editorStore';
import { useConnectErrors } from '@/features/connections/connectErrors';
import { useSessionHealthStore } from '@/stores/sessionHealth';
import { useTabsStore } from '@/stores/tabs';
import { collectWork, type WorkItem } from './model';

/** Unfinished work across every workspace (re-computed when tabs, editors or edits change). */
export function useWork(): WorkItem[] {
  const tabs = useTabsStore((s) => s.tabs);
  const scope = useTabsStore((s) => s.scope);
  const stashed = useTabsStore((s) => s.stashed);
  const editors = useEditorStore((s) => s.tabs);
  const pending = usePendingChanges((s) => s.byTab);
  return useMemo(
    () => collectWork({ tabs, scope, stashed, editors, pending }),
    [tabs, scope, stashed, editors, pending],
  );
}

/**
 * Badge of the Launchpad button: open transactions (they hold locks) plus connections that were
 * lost or failed to connect.
 */
export function useAttentionCount(): number {
  const work = useWork();
  const health = useSessionHealthStore((s) => s.byConnection);
  const errors = useConnectErrors((s) => s.byConnection);
  const transactions = work.filter((w) => w.kind === 'transaction').length;
  const lost = Object.values(health).filter(
    (h) => h.state === 'lost' || h.state === 'gaveUp',
  ).length;
  return transactions + lost + Object.keys(errors).length;
}
