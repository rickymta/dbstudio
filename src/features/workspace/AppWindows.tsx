import { useEffect } from 'react';
import { blockBrowserKeys, shouldBlockBrowserKeys } from '@/app/browserKeys';
import { ActivityWindow } from '@/features/activity/ActivityWindow';
import { matchesShortcut } from '@/features/editor/shortcuts';
import { HelpDialog } from '@/features/help/HelpDialog';
import { EVENTS, ipc } from '@/lib/ipc';
import type { SessionStateEvent } from '@/lib/types';
import { useActivity } from '@/stores/activity';
import { useActivityWindow, useHelpDialog } from './paletteStore';
import { ZOOM_STEP, applyZoom, useZoom } from './zoom';

/** Session health changes worth a line in the activity log. */
const HEALTH_EVENT: Partial<Record<SessionStateEvent['state'], string>> = {
  lost: 'connectionLost',
  reconnected: 'connectionRestored',
  gaveUp: 'reconnectGaveUp',
};

/**
 * App-wide windows and their shortcuts, mounted once: Activity Logs (Ctrl+Shift+L), Help &
 * Support, and the interface zoom (Ctrl + / - / 0). The key handler runs in the capture phase so
 * the combos also work while the SQL editor has the focus.
 */
export function AppWindows() {
  const activityOpen = useActivityWindow((s) => s.open);
  const setActivityOpen = useActivityWindow((s) => s.setOpen);
  const helpOpen = useHelpDialog((s) => s.open);
  const setHelpOpen = useHelpDialog((s) => s.setOpen);
  const zoom = useZoom((s) => s.zoom);

  useEffect(() => applyZoom(zoom), [zoom]);

  // F5 and the other browser keys must not reload or inspect the packaged app.
  useEffect(() => (shouldBlockBrowserKeys() ? blockBrowserKeys() : undefined), []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const { zoom: z, setZoom } = useZoom.getState();
      if (matchesShortcut(e, 'zoomIn')) setZoom(z + ZOOM_STEP);
      else if (matchesShortcut(e, 'zoomOut')) setZoom(z - ZOOM_STEP);
      else if (matchesShortcut(e, 'zoomReset')) setZoom(1);
      else if (matchesShortcut(e, 'activity')) {
        const { open, setOpen } = useActivityWindow.getState();
        // Another modal dialog keeps the keyboard; the log itself toggles.
        if (!open && document.querySelector('[role="dialog"]')) return;
        setOpen(!open);
      } else return;
      e.preventDefault();
      e.stopPropagation();
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, []);

  // Application start and connection health go to the activity log.
  useEffect(() => {
    let off: (() => void) | undefined;
    let alive = true;
    void ipc
      .onEvent<SessionStateEvent>(EVENTS.sessionState, (e) => {
        const event = HEALTH_EVENT[e.state];
        if (!event) return;
        useActivity.getState().log({
          connectionId: e.connectionId,
          event,
          status: e.state === 'reconnected' ? 'info' : 'failed',
          ...(e.error ? { error: e.error.message } : {}),
        });
      })
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
      <ActivityWindow open={activityOpen} onOpenChange={setActivityOpen} />
      <HelpDialog open={helpOpen} onOpenChange={setHelpOpen} />
    </>
  );
}
