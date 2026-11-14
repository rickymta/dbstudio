import { Bug, X } from 'lucide-react';
import { useEffect, type FC, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';
import { closeToolWindow, ensureDebugEvents, loadKeymap, requestStop } from './debugActions';
import { isLive, useDebugStore } from './debugStore';
import { BreakpointPopover } from './BreakpointPopover';
import { DebugDialogs } from './DebugDialogs';
import { DebugToolWindow } from './DebugToolWindow';

export interface DebugBottomHostProps {
  tabStateId: string;
  /** The results panel the tool window replaces or sits beside while a session is live. */
  children: ReactNode;
}

/**
 * Bottom area of an editor tab: the results, plus the Debug tool window while a session exists
 * (a second tab beside "Results"; both stay mounted so results keep their state).
 */
export const DebugBottomHost: FC<DebugBottomHostProps> = ({ tabStateId, children }) => {
  const { t } = useTranslation('debugger');
  const session = useDebugStore((s) => s.sessions[tabStateId]);
  const bottom = useDebugStore((s) => s.ui[tabStateId]?.bottom ?? 'results');
  const live = isLive(session);

  useEffect(() => {
    void ensureDebugEvents();
    void loadKeymap();
  }, []);

  const select = (view: 'results' | 'debug') =>
    useDebugStore.getState().patchUi(tabStateId, { bottom: view });
  const showDebug = !!session && bottom === 'debug';

  return (
    <div className="flex h-full min-h-0 flex-col">
      {session && (
        <div
          role="tablist"
          aria-label={t('host.tabs')}
          className="flex h-8 shrink-0 items-end gap-3 border-b border-border bg-surface px-2"
        >
          <button
            role="tab"
            type="button"
            aria-selected={!showDebug}
            className={cn(
              '-mb-px h-8 border-b-2 px-1 text-[13px]',
              !showDebug
                ? 'border-accent text-foreground'
                : 'border-transparent text-muted-foreground',
            )}
            onClick={() => select('results')}
          >
            {t('host.results')}
          </button>
          <span className="-mb-px inline-flex items-center">
            <button
              role="tab"
              type="button"
              aria-selected={showDebug}
              className={cn(
                'inline-flex h-8 items-center gap-1 border-b-2 px-1 text-[13px]',
                showDebug
                  ? 'border-accent text-foreground'
                  : 'border-transparent text-muted-foreground',
              )}
              onClick={() => select('debug')}
            >
              <Bug size={13} aria-hidden /> {t('host.debug')}
              {live && (
                <span className="size-1.5 rounded-full bg-success" aria-label={t('host.live')} />
              )}
            </button>
            <button
              type="button"
              className="ml-0.5 rounded p-0.5 text-muted-foreground hover:bg-surface-muted hover:text-foreground"
              aria-label={live ? t('window.stopAndClose') : t('window.close')}
              onClick={() => (live ? requestStop(tabStateId) : closeToolWindow(tabStateId))}
            >
              <X size={12} aria-hidden />
            </button>
          </span>
          {live && (
            <span className="ml-auto pb-1.5 text-xs text-muted-foreground">
              {t('host.readOnly')}
            </span>
          )}
        </div>
      )}
      <div className={cn('min-h-0 flex-1', showDebug && 'hidden')}>{children}</div>
      {showDebug && (
        <div className="min-h-0 flex-1">
          <DebugToolWindow tabId={tabStateId} />
        </div>
      )}
      <BreakpointPopover match={(scope) => scope.tabId === tabStateId} moreTabId={tabStateId} />
      <DebugDialogs tabId={tabStateId} />
    </div>
  );
};
