// The Debug tool window of an editor tab (design 5.1): session toolbar | Debugger (Frames,
// Variables, Watches) / Console tabs with the step bar, status line and banners.

import { TriangleAlert, X } from 'lucide-react';
import { useEffect, useState, type FC, type KeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Group, Panel, Separator } from 'react-resizable-panels';
import { Button } from '@/components/ui/Button';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/Tabs';
import { cn } from '@/lib/cn';
import {
  closeToolWindow,
  confirmPending,
  requestStop,
  runCommand,
  stopDebug,
} from './debugActions';
import { isLive, useDebugStore, type ToolPanel } from './debugStore';
import { useDebugUi } from './debugUiStore';
import { ConsolePanel } from './ConsolePanel';
import { FramesPanel } from './FramesPanel';
import { commandOfEvent } from './keymap';
import { statusText } from './status';
import { SessionToolbar, StepBar } from './DebugToolbar';
import { VariablesPanel } from './VariablesPanel';
import { WatchesPanel } from './WatchesPanel';

// Several windows may show one session: each confirmation token is asked once, app-wide.
const askedTokens = new Set<string>();

const fmtDuration = (ms: number): string => {
  const s = Math.floor(ms / 1000);
  return s >= 60 ? `${Math.floor(s / 60)}m ${s % 60}s` : `${s}s`;
};

const fmtClock = (secs: number): string =>
  `${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, '0')}`;

const Banner: FC<{ tone: 'warning' | 'danger' | 'info'; children: React.ReactNode }> = ({
  tone,
  children,
}) => (
  <div
    role="status"
    className={cn(
      'flex shrink-0 items-center gap-2 border-t border-border px-3 py-1 text-xs',
      tone === 'warning' && 'bg-warning-soft text-foreground',
      tone === 'danger' && 'bg-danger-soft text-foreground',
      tone === 'info' && 'bg-surface-muted',
    )}
  >
    {children}
  </div>
);

export const DebugToolWindow: FC<{
  tabId: string;
  /** A second view of the same session (routine tab). */
  passive?: boolean;
}> = ({ tabId, passive = false }) => {
  const { t } = useTranslation('debugger');
  const session = useDebugStore((s) => s.sessions[tabId]);
  const keymap = useDebugStore((s) => s.keymap);
  const panel = useDebugStore((s) => s.ui[tabId]?.panel ?? 'debugger');
  const [warnDismissed, setWarnDismissed] = useState(false);
  const state = session?.state;
  const live = isLive(session);

  // A destructive/production statement waits for the typed confirmation: ask once per token.
  const reason = state?.status.kind === 'paused' ? state.status.reason : null;
  const token = reason?.kind === 'confirmation' ? reason.request.token : null;
  useEffect(() => {
    if (token && !askedTokens.has(token)) {
      askedTokens.add(token);
      void confirmPending(tabId);
    }
  }, [tabId, token]);

  if (!session || !state) return null;
  const status = state.status;
  const paused = status.kind === 'paused';
  const tx = state.transaction;
  const showWarn = paused && tx.warn && !warnDismissed;
  const ttlLeft = paused ? tx.ttlLeftSecs : null;

  const onKeyDown = (e: KeyboardEvent) => {
    const command = commandOfEvent(e.nativeEvent, keymap, live);
    if (!command) return;
    e.preventDefault();
    runCommand(tabId, command);
  };

  return (
    <div
      className="flex h-full min-h-0 flex-col bg-surface"
      data-testid="debug-tool-window"
      role="region"
      aria-label={t('window.label')}
      onKeyDown={onKeyDown}
    >
      <div className="flex min-h-0 flex-1">
        <SessionToolbar tabId={tabId} />
        <Tabs
          value={panel}
          onValueChange={(v) => useDebugStore.getState().patchUi(tabId, { panel: v as ToolPanel })}
          className="flex min-w-0 flex-1 flex-col"
        >
          <div className="flex shrink-0 items-center gap-2 border-b border-border pr-2">
            <TabsList className="border-b-0">
              <TabsTrigger value="debugger">{t('window.debugger')}</TabsTrigger>
              <TabsTrigger value="console">{t('window.console')}</TabsTrigger>
            </TabsList>
            <StepBar tabId={tabId} />
            <span className="flex-1" />
            <span
              className="min-w-0 truncate text-xs text-muted-foreground"
              data-testid="debug-status"
              title={statusText(t, state)}
            >
              {statusText(t, state)}
            </span>
            {session.lastStepMs !== null && (
              <span className="shrink-0 text-xs text-muted-foreground">
                {session.lastStepMs} ms
              </span>
            )}
            {tx.owned && (
              <span className="shrink-0 rounded bg-surface-muted px-1.5 py-px text-xs">
                {t('window.txOwned')}
              </span>
            )}
            {ttlLeft != null && ttlLeft < 120 && (
              <span className="shrink-0 text-xs text-warning">
                {t('window.ttl', { time: fmtClock(ttlLeft) })}
              </span>
            )}
            <Button
              size="icon"
              variant="ghost"
              className="size-6 min-w-6"
              aria-label={live ? t('window.stopAndClose') : t('window.close')}
              onClick={() => (live ? requestStop(tabId) : closeToolWindow(tabId))}
            >
              <X size={14} aria-hidden />
            </Button>
          </div>
          <TabsContent
            value="debugger"
            forceMount
            className="min-h-0 flex-1 data-[state=inactive]:hidden"
          >
            <Group orientation="horizontal" id={`debug-panels-${passive ? 'src-' : ''}${tabId}`}>
              <Panel id="frames" defaultSize="22%" minSize="10%">
                <FramesPanel tabId={tabId} />
              </Panel>
              <Separator className="w-1 bg-border/50 transition-colors hover:bg-accent data-[separator=active]:bg-accent" />
              <Panel id="variables" defaultSize="46%" minSize="15%">
                <VariablesPanel tabId={tabId} />
              </Panel>
              <Separator className="w-1 bg-border/50 transition-colors hover:bg-accent data-[separator=active]:bg-accent" />
              <Panel id="watches" defaultSize="32%" minSize="10%">
                <WatchesPanel tabId={tabId} />
              </Panel>
            </Group>
          </TabsContent>
          <TabsContent
            value="console"
            forceMount
            className="min-h-0 flex-1 data-[state=inactive]:hidden"
          >
            <ConsolePanel tabId={tabId} />
          </TabsContent>
        </Tabs>
      </div>
      <div aria-live="polite" className="sr-only" data-testid="debug-live">
        {statusText(t, state)}
      </div>
      {showWarn && (
        <Banner tone="warning">
          <TriangleAlert size={14} className="shrink-0 text-warning" aria-hidden />
          <span className="flex-1">
            {t('banner.txWarn', { time: fmtDuration(tx.openForMs ?? 0) })}
          </span>
          <Button size="sm" variant="danger" onClick={() => void stopDebug(tabId, 'rollback')}>
            {t('banner.rollbackStop')}
          </Button>
          <Button size="sm" onClick={() => setWarnDismissed(true)}>
            {t('banner.continue')}
          </Button>
        </Banner>
      )}
      {reason?.kind === 'confirmation' && (
        <Banner tone="warning">
          <TriangleAlert size={14} className="shrink-0 text-warning" aria-hidden />
          <span className="flex-1">{t('banner.confirmation')}</span>
          <Button size="sm" onClick={() => void confirmPending(tabId)}>
            {t('banner.confirm')}
          </Button>
        </Banner>
      )}
      {reason?.kind === 'blocked' && (
        <Banner tone="danger">
          <TriangleAlert size={14} className="shrink-0 text-danger" aria-hidden />
          <span className="flex-1">{t('banner.blocked', { message: reason.error.message })}</span>
        </Banner>
      )}
      {reason?.kind === 'awaitingEnd' && (
        <Banner tone="info">
          <span className="flex-1">{t('banner.awaitingEnd')}</span>
          <Button
            size="sm"
            variant="primary"
            onClick={() => useDebugUi.getState().openDialog(tabId, { kind: 'end', then: 'stop' })}
          >
            {t('banner.chooseEnd')}
          </Button>
        </Banner>
      )}
      {status.kind === 'terminated' && (
        <Banner tone="info">
          <span className="flex-1" data-testid="debug-report">
            {status.report
              ? t('banner.report', {
                  steps: status.report.stepsRun,
                  outcome: status.report.committed
                    ? t('banner.committed')
                    : status.report.rolledBack
                      ? t('banner.rolledBack')
                      : t('banner.noTx'),
                })
              : t(`status.terminated.${status.reason}`)}
          </span>
          <Button size="sm" onClick={() => closeToolWindow(tabId)}>
            {t('window.close')}
          </Button>
        </Banner>
      )}
    </div>
  );
};
