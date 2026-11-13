// App-level pieces of the editor feature, mounted once (the status bar renders it): the tab
// restore and persistence, `lease://state` events, global shortcuts, and the dialogs that must
// work whichever tab is active (close with open transaction, destructive confirmation,
// shortcut cheat-sheet). The tab registry only renders the active tab, so none of this can
// live inside `EditorTab`.

import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { ConfirmDestructiveHost } from '@/features/safety/ConfirmDestructiveHost';
import { ipc } from '@/lib/ipc';
import type { LeaseStateEvent } from '@/lib/types';
import { useTabsStore } from '@/stores/tabs';
import { applyLeaseState, setCloseTransactionAsker } from './editorRunner';
import { openEditorForActiveTab } from './open';
import { restoreEditorTabsOnce, startEditorPersistence } from './persist';
import { ShortcutsDialog } from './ShortcutsDialog';
import { useShortcutsHelp } from './shortcutsHelpStore';
import { SHORTCUTS, matchesKeys } from './shortcuts';

type CloseAnswer = 'commit' | 'rollback' | 'cancel';

function CloseTransactionDialog() {
  const { t } = useTranslation('editor');
  const [pending, setPending] = useState<{ resolve: (a: CloseAnswer) => void } | null>(null);
  useEffect(() => {
    setCloseTransactionAsker(() => new Promise<CloseAnswer>((resolve) => setPending({ resolve })));
    return () => setCloseTransactionAsker(() => Promise.resolve('cancel'));
  }, []);
  const answer = (a: CloseAnswer) => {
    pending?.resolve(a);
    setPending(null);
  };
  return (
    <Dialog open={pending !== null} onOpenChange={(o) => !o && answer('cancel')}>
      <DialogContent
        title={t('close.title')}
        description={t('close.description')}
        footer={
          <>
            <Button onClick={() => answer('cancel')}>{t('close.cancel')}</Button>
            <Button variant="danger" onClick={() => answer('rollback')}>
              {t('close.rollback')}
            </Button>
            <Button variant="primary" onClick={() => answer('commit')}>
              {t('close.commit')}
            </Button>
          </>
        }
      />
    </Dialog>
  );
}

function useGlobalShortcuts(): void {
  useEffect(() => {
    const globals = SHORTCUTS.filter((s) => s.scope === 'global');
    const onKey = (e: KeyboardEvent) => {
      const hit = globals.find((s) => matchesKeys(e, s.keys));
      if (!hit) return;
      switch (hit.id) {
        case 'newTab':
          if (openEditorForActiveTab()) e.preventDefault();
          break;
        case 'closeTab': {
          const { activeId, tabs, requestClose } = useTabsStore.getState();
          if (tabs.find((t) => t.id === activeId)?.closable) {
            e.preventDefault();
            void requestClose(activeId);
          }
          break;
        }
        case 'findObject': {
          const input = document.querySelector<HTMLInputElement>('[data-search-input]');
          if (input) {
            e.preventDefault();
            input.focus();
            input.select();
          }
          break;
        }
        case 'help':
          e.preventDefault();
          useShortcutsHelp.getState().setOpen(true);
          break;
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);
}

export function EditorHost() {
  useGlobalShortcuts();

  // Reopen the persisted tabs once, then keep them saved.
  useEffect(() => {
    const restored = restoreEditorTabsOnce();
    let stop: (() => void) | undefined;
    let alive = true;
    void restored.then(() => {
      if (alive) stop = startEditorPersistence();
    });
    return () => {
      alive = false;
      stop?.();
    };
  }, []);

  // `lease://state`: the backend reports a lost connection (CONN-12).
  useEffect(() => {
    let off: (() => void) | undefined;
    let alive = true;
    void ipc
      .onEvent<LeaseStateEvent>('lease://state', (e) => applyLeaseState(e.leaseId, e.state))
      .then((unlisten) => {
        if (alive) off = unlisten;
        else unlisten();
      });
    return () => {
      alive = false;
      off?.();
    };
  }, []);

  return (
    <>
      <CloseTransactionDialog />
      <ConfirmDestructiveHost />
      <ShortcutsDialog />
    </>
  );
}
