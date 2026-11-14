import { PostgreSQL, sql } from '@codemirror/lang-sql';
import { EditorState } from '@codemirror/state';
import { EditorView, lineNumbers } from '@codemirror/view';
import { useEffect, useMemo, useRef, useState, type FC } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Spinner } from '@/components/ui/Spinner';
import { editorHighlight, editorTheme } from '@/features/editor/cmTheme';
import { ipc } from '@/lib/ipc';
import type { IpcError, RoutineSource } from '@/lib/types';
import { useTabsStore, editorTabId, type DebugSourcePayload } from '@/stores/tabs';
import { createSourceExtension } from './cm/extension';
import { BreakpointPopover } from './BreakpointPopover';
import { ensureDebugEvents, ensureRoutineScope } from './debugActions';
import { routineScope, tabOfRoutine, useDebugStore } from './debugStore';
import { DebugDialogs } from './DebugDialogs';
import { DebugToolWindow } from './DebugToolWindow';
import { StepBar } from './DebugToolbar';

/** Read-only tab with the source of a routine the debugger stopped in (or will stop in). */
const DebugSourceTab: FC<DebugSourcePayload> = ({ sessionId, connectionId, signature }) => {
  const { t } = useTranslation('debugger');
  const host = useRef<HTMLDivElement>(null);
  const [source, setSource] = useState<RoutineSource | null>(null);
  const [error, setError] = useState<IpcError | null>(null);
  const scopeKey = routineScope(connectionId, signature);
  const sessionTab = useDebugStore(() => tabOfRoutine(signature));
  const hasSession = useDebugStore((s) => !!sessionTab && !!s.sessions[sessionTab]);
  const hasScope = useDebugStore((s) => !!s.scopes[scopeKey]);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        await ensureDebugEvents();
        const routine = { signature };
        const loaded = await ipc.debugRoutineSource(
          tabOfRoutine(signature)
            ? (useDebugStore.getState().sessions[tabOfRoutine(signature)!]?.debugId ?? null)
            : null,
          sessionId,
          routine,
        );
        await ensureRoutineScope(connectionId, routine, loaded.sourceHash);
        if (!cancelled) setSource(loaded);
      } catch (e) {
        if (!cancelled) setError(e as IpcError);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [sessionId, connectionId, signature]);

  const extensions = useMemo(
    () =>
      source && hasScope
        ? createSourceExtension({
            scopeKey,
            lineBase: source.firstLine,
            tabIdOf: () => tabOfRoutine(signature),
          })
        : null,
    [source, hasScope, scopeKey, signature],
  );

  useEffect(() => {
    if (!source || !extensions || !host.current) return;
    const base = source.firstLine;
    const view = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: source.text,
        extensions: [
          lineNumbers({ formatNumber: (n) => String(n + base - 1) }),
          sql({ dialect: PostgreSQL }),
          editorTheme,
          editorHighlight,
          EditorState.readOnly.of(true),
          EditorView.contentAttributes.of({
            'aria-label': t('source.label', { signature }),
            'aria-readonly': 'true',
          }),
          extensions,
        ],
      }),
    });
    return () => view.destroy();
  }, [source, extensions, signature, t]);

  const backToScript = () => {
    if (!sessionTab) return;
    useTabsStore.getState().activate(editorTabId(sessionTab));
  };

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="debug-source-tab">
      <div className="flex min-h-10 shrink-0 flex-wrap items-center gap-3 border-b border-border px-3 py-1">
        <span className="font-mono text-[13px]">{signature}</span>
        {source && <span className="text-xs text-muted-foreground">{source.language}</span>}
        <span className="flex-1" />
        {sessionTab && (
          <>
            <StepBar tabId={sessionTab} scopeKey={scopeKey} />
            <Button size="sm" onClick={backToScript}>
              {t('source.backToScript')}
            </Button>
          </>
        )}
      </div>
      {error && <p className="p-3 text-[13px] text-danger">{error.message}</p>}
      {!source && !error && (
        <div className="flex flex-1 items-center justify-center">
          <Spinner />
        </div>
      )}
      <div ref={host} className="min-h-0 flex-1" data-testid="debug-source-editor" />
      {sessionTab && hasSession && (
        <div className="h-[40%] min-h-48 shrink-0 border-t border-border">
          <DebugToolWindow tabId={sessionTab} passive />
        </div>
      )}
      <BreakpointPopover
        match={(scope) => scope.routine?.signature === signature}
        moreTabId={sessionTab}
      />
      {sessionTab && <DebugDialogs tabId={sessionTab} />}
    </div>
  );
};

export default DebugSourceTab;
