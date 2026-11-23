import { useWorkspaceStore } from '@/features/workspace/layout';
import { useLaunchpad } from '@/features/launchpad/store';
// App-level pieces of the editor feature, mounted once (by `App`): the tab
// restore and persistence, `lease://state` events, global shortcuts, and the dialogs that must
// work whichever tab is active (close with open transaction, shortcut cheat-sheet). The tab registry only renders the active tab, so none of this can
// live inside `EditorTab`.

import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { ipc } from '@/lib/ipc';
import type { LeaseStateEvent } from '@/lib/types';
import { useTabsStore } from '@/stores/tabs';
import { applyLeaseState, setCloseTransactionAsker } from './editorRunner';
import { openEditorForActiveTab } from './open';
import { restoreEditorTabsOnce, startEditorPersistence } from './persist';
import { ShortcutsDialog } from './ShortcutsDialog';
import { useShortcutsHelp } from './shortcutsHelpStore';
import { useCommandPalette } from '@/features/workspace/paletteStore';
import { SHORTCUTS, matchesKeys } from './shortcuts';

type CloseAnswer = 'commit' | 'rollback' | 'cancel';

function CloseTransactionDialog() {
  const { t } = useTranslation('editor');
  // Requests queue up: a second close while the dialog is open waits its turn, never hangs.
  const [queue, setQueue] = useState<{ resolve: (a: CloseAnswer) => void }[]>([]);
  useEffect(() => {
    setCloseTransactionAsker(
      () => new Promise<CloseAnswer>((resolve) => setQueue((q) => [...q, { resolve }])),
    );
    return () => setCloseTransactionAsker(() => Promise.resolve('cancel'));
  }, []);
  const head = queue[0];
  const answer = (a: CloseAnswer) => {
    head?.resolve(a);
    setQueue((q) => q.slice(1));
  };
  return (
    <Dialog open={head !== undefined} onOpenChange={(o) => !o && answer('cancel')}>
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
      // Already handled (the tab bar closes its own tab on Ctrl+W; the editor handles its keys),
      // or a modal dialog has the focus: leave it alone.
      if (e.defaultPrevented) return;
      const hit = globals.find((s) => matchesKeys(e, s.keys));
      if (!hit) return;
      // Ctrl+F never reaches the webview: its "find in page" bar is not part of the app.
      if (hit.id === 'spotlight') e.preventDefault();
      if (document.querySelector('[role="dialog"]')) return;
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
        case 'toggleSidebar':
          e.preventDefault();
          useWorkspaceStore.getState().toggleSidebar();
          break;
        case 'findObject': {
          useWorkspaceStore.getState().showSidebar();
          const input = document.querySelector<HTMLInputElement>('[data-search-input]');
          if (input) {
            e.preventDefault();
            input.focus();
            input.select();
          }
          break;
        }
        case 'launchpad':
          e.preventDefault();
          useLaunchpad.getState().toggle();
          break;
        case 'palette':
        case 'spotlight':
          e.preventDefault();
          useCommandPalette.getState().setOpen(true);
          break;
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
    void restored.then((count) => {
      // A failed restore must not enable saving: it would overwrite the stored tabs.
      if (alive && count !== null) stop = startEditorPersistence();
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
      <ShortcutsDialog />
    </>
  );
}
