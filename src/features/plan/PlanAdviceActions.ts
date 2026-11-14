// What the buttons of an advice card do. Nothing here runs a statement: text is copied, or put
// in an editor for the user to review and run (through the usual guard).

import { useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from '@/components/ui/toastStore';
import { appendToEditor, openEditor } from '@/features/editor/open';
import { useEditorStore } from '@/features/editor/editorStore';
import { openObjectTab } from '@/features/explorer/openObject';
import { useConnectionProfile } from '@/features/connections/queries';
import type { AdviceAction, ConnectionProfile, PlanAdvice, SessionInfo } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { editorTabId, useTabsStore } from '@/stores/tabs';
import { actionText } from './adviceModel';
import { usePlanStore } from './planStore';

export interface AdviceActions {
  copy: (action: AdviceAction) => void;
  insert: (action: AdviceAction) => void;
  openTable: (advice: PlanAdvice) => void;
  /** The connection of the plan is open, so "Open table" can work. */
  canOpenTable: boolean;
  profile: ConnectionProfile | undefined;
  readOnly: boolean;
  production: boolean;
}

export function useAdviceActions(planId: string): AdviceActions {
  const { t } = useTranslation('plan');
  const connectionId = usePlanStore((s) => s.entries[planId]?.connectionId ?? '');
  const profile = useConnectionProfile(connectionId);
  const session = useSessionsStore((s) => s.sessionsByConnection[connectionId]);

  const copy = useCallback(
    (action: AdviceAction) => {
      const text = actionText(action);
      if (!text) return;
      void navigator.clipboard
        ?.writeText(text)
        .then(() => toast.success(t('advice.copied')))
        .catch(() => toast.error(t('advice.copyFailed')));
    },
    [t],
  );

  const insert = useCallback(
    (action: AdviceAction) => {
      const text = actionText(action);
      const entry = usePlanStore.getState().entries[planId];
      if (!text || !entry) return;
      const select = !action.template;
      // The editor the plan came from, when it is still open on the same session.
      const source = entry.sourceEditorId;
      if (source) {
        const tab = useTabsStore.getState().tabs.find((x) => x.id === editorTabId(source));
        if (
          tab?.payload?.sessionId === entry.sessionId &&
          appendToEditor(source, text, { select })
        ) {
          return;
        }
      }
      const live: SessionInfo | undefined =
        useSessionsStore.getState().sessionsByConnection[entry.connectionId];
      if (!live) {
        toast.error(t('advice.noSession'));
        return;
      }
      const id = openEditor(live, { text, database: entry.request?.database ?? undefined });
      if (select) {
        useEditorStore.getState().patch(id, { selection: { from: 0, to: text.length } });
      }
    },
    [planId, t],
  );

  const openTable = useCallback(
    (advice: PlanAdvice) => {
      const path = advice.table?.path;
      const live =
        useSessionsStore.getState().sessionsByConnection[
          usePlanStore.getState().entries[planId]?.connectionId ?? ''
        ];
      if (!path || !live) return;
      const last = path.segments[path.segments.length - 1];
      openObjectTab(
        {
          path,
          kind: last?.kind ?? 'table',
          name: last?.name ?? advice.table?.name ?? '',
          hasChildren: false,
        },
        live,
      );
    },
    [planId],
  );

  return {
    copy,
    insert,
    openTable,
    canOpenTable: session !== undefined,
    profile,
    readOnly: profile?.readOnly === true,
    production: profile?.environment === 'production',
  };
}
