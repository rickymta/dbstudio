import { DebugBottomHost, DebugButton, debuggerExtensions } from '@/features/debugger';
import { useSavedLayout } from '@/features/workspace/layout';
import { utf16ToCharOffset, charToUtf16Offset } from '@/lib/text';
import {
  CircleHelp,
  FileDown,
  FolderOpen,
  History,
  ListTree,
  ListVideo,
  Play,
  Save,
  Square,
  WandSparkles,
  PlugZap,
} from 'lucide-react';
import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState, type FC } from 'react';
import { useTranslation } from 'react-i18next';
import { Group, Panel, Separator } from 'react-resizable-panels';
import { Button } from '@/components/ui/Button';
import { IconButton } from '@/components/ui/IconButton';
import { Spinner } from '@/components/ui/Spinner';
import { useToastStore } from '@/components/ui/toastStore';
import { useConnectionProfile } from '@/features/connections/queries';
import { useSessionOpen } from '@/features/connections/useSessionOpen';
import { useSessionReconnect } from '@/features/connections/useSessionReconnect';
import { ipc, toIpcError } from '@/lib/ipc';
import type { ParamRef, ParamValue, ScriptAnalysis } from '@/lib/types';
import { useIsOpening, useSession } from '@/stores/sessions';
import { isSessionLost } from '@/stores/sessionRecovery';
import type { EditorTabPayload } from '@/stores/tabs';
import { analyzeTab, cancelRun, fetchMore, registerEditorTab, runText } from './editorRunner';
import {
  isDirty,
  patchTabData,
  useEditorStore,
  useTabData,
  type EditorTabData,
} from './editorStore';
import { openPlan } from '@/features/plan/open';
import { invalidateCatalog } from './catalogLoader';
import { ConnectionPicker } from './ConnectionPicker';
import { DatabasePicker } from './DatabasePicker';
import { ConsoleView } from './ConsoleView';
import { isSqlDialect } from './dialects';
import { formatSql } from './format';
import { addEditorTab, fileBaseName, nextEditorTitle, setEditorTitle } from './open';
import { ParamsDialog } from './ParamsDialog';
import { toParamValue } from './params';
import { ResultsPanel } from './ResultsPanel';
import { defaultScope, selectRunText, statementAt, type RunScope } from './runScope';
import { keysOf, formatKeys, type ShortcutId } from './shortcuts';
import { useShortcutsHelp } from './shortcutsHelpStore';
import { SidePanel } from './SidePanel';
import type { SqlEditorHandle } from './SqlEditor';
import { TxBar } from './TxBar';

// CodeMirror is a separate chunk, fetched when the first editor opens.
const SqlEditor = lazy(() => import('./SqlEditor'));

const ANALYZE_DELAY_MS = 600;
const LIVE_ANALYZE_MAX_CHARS = 1_000_000;
const DDL_PATTERN = /\b(create|alter|drop|truncate|rename)\b/i;

export const EditorTab: FC<EditorTabPayload> = ({ tabStateId, connectionId, mode }) => {
  const data = useTabData(tabStateId);
  useEffect(() => {
    useEditorStore.getState().ensure(tabStateId);
    registerEditorTab(tabStateId);
  }, [tabStateId]);
  if (!data) return null;
  return <EditorBody data={data} connectionId={connectionId} mode={mode} />;
};

function EditorBody({
  data,
  connectionId,
  mode,
}: {
  data: EditorTabData;
  connectionId: string | null;
  mode: EditorTabPayload['mode'];
}) {
  const { t } = useTranslation('editor');
  const id = data.id;
  const session = useSession(connectionId ?? '');
  const opening = useIsOpening(connectionId ?? '');
  const profile = useConnectionProfile(connectionId ?? '');
  // Password and SSH host key prompts of the Connect button.
  const { open: openSession, dialog: openDialog } = useSessionOpen();
  const { reconnect: reconnectWithPrompts, dialog: reconnectDialog } = useSessionReconnect();
  const toast = useToastStore.getState().push;
  const editor = useRef<SqlEditorHandle>(null);
  const panel = data.historyOpen;
  const setPanel = (update: boolean | ((value: boolean) => boolean)) =>
    patchTabData(id, {
      historyOpen: typeof update === 'function' ? update(data.historyOpen) : update,
    });
  const layout = useSavedLayout('editor');
  const resultsRef = useRef<HTMLDivElement>(null);
  const [paramsFor, setParamsFor] = useState<{
    refs: ParamRef[];
    run: (p: ParamValue[]) => void;
  } | null>(null);

  const [dbPickerAvailable, setDbPickerAvailable] = useState(false);
  const sessionId = session?.sessionId ?? null;
  const dialect = session?.dialect ?? {
    name: mode === 'mongo' ? 'mongoShell' : mode === 'redis' ? 'redisCli' : 'opaque',
  };
  const run = data.run;
  // Fetching more rows of the last result keeps the lease busy like a running statement.
  const running = run?.status === 'running' || run?.fetchingMore === true;
  const caps = session?.descriptor.capabilities;
  const showTx = mode === 'sql' && (caps?.manualTransactions ?? true);

  // Lexer problems (unterminated string...) while typing.
  useEffect(() => {
    // Huge scripts are not analyzed while typing (the text would cross IPC on every pause).
    if (!sessionId || mode === 'redis' || data.text.length > LIVE_ANALYZE_MAX_CHARS) return;
    const timer = setTimeout(() => void analyzeTab(id, sessionId, data.text), ANALYZE_DELAY_MS);
    return () => clearTimeout(timer);
  }, [id, sessionId, mode, data.text]);

  // The mark's position is only right for the document that ran.
  const errorMark = run?.errorMark && data.text === run.docAtStart ? run.errorMark : null;
  const marks = useMemo(
    () => [...(errorMark ? [errorMark] : []), ...data.problems],
    [errorMark, data.problems],
  );

  /** Bring the lost session back (same session, or a fresh one when the backend forgot it). */
  const reconnect = useCallback(
    (lostSessionId: string) => {
      if (!connectionId) return;
      void reconnectWithPrompts(lostSessionId, connectionId)
        .then(() => toast('success', t('connection.reconnected')))
        .catch((e: unknown) =>
          toast('error', t('connection.reconnectFailed', { message: toIpcError(e).message })),
        );
    },
    [connectionId, reconnectWithPrompts, t, toast],
  );

  /**
   * Analyze the script before running it. A failure is reported, never swallowed: a lost session
   * gets a toast with a reconnect button, anything else its own message. `null` = reported.
   */
  const analyzeForRun = useCallback(
    async (sid: string, text: string): Promise<ScriptAnalysis | null> => {
      try {
        return await ipc.scriptAnalyze(sid, text);
      } catch (e) {
        if (isSessionLost(e)) {
          toast('error', t('connection.sessionLost'), 15000, {
            label: t('connection.reconnect'),
            onClick: () => reconnect(sid),
          });
        } else {
          toast('error', toIpcError(e).message);
        }
        return null;
      }
    },
    [reconnect, t, toast],
  );

  const start = useCallback(
    async (scope: RunScope) => {
      if (!sessionId) {
        toast('info', t('connection.notConnected'));
        return;
      }
      editor.current?.flush();
      const d = useEditorStore.getState().tabs[id];
      if (!d) return;
      const wholeAnalysis = await analyzeForRun(sessionId, d.text);
      if (!wholeAnalysis) return;
      const input = selectRunText(d.text, scope, d.selection, wholeAnalysis.statements);
      if (!input) {
        toast('info', t('run.nothing'));
        return;
      }
      const analysis =
        scope === 'all'
          ? wholeAnalysis
          : await ipc.scriptAnalyze(sessionId, input.text).catch(() => null);
      const go = async (params: ParamValue[]) => {
        const snap = await runText(id, sessionId, { ...input, params });
        if (snap && DDL_PATTERN.test(input.text)) invalidateCatalog(sessionId);
      };
      if (analysis && analysis.params.length > 0)
        setParamsFor({ refs: analysis.params, run: (p) => void go(p) });
      else await go([]);
    },
    [id, sessionId, t, toast, analyzeForRun],
  );

  /** Explain the selection or the statement at the cursor in a plan tab (no ANALYZE). */
  const explain = useCallback(async () => {
    if (!sessionId || !connectionId) {
      toast('info', t('connection.notConnected'));
      return;
    }
    editor.current?.flush();
    const d = useEditorStore.getState().tabs[id];
    if (!d) return;
    const whole = await analyzeForRun(sessionId, d.text);
    if (!whole) return;
    const input = selectRunText(d.text, defaultScope(d.selection), d.selection, whole.statements);
    if (!input) {
      toast('info', t('run.nothing'));
      return;
    }
    openPlan({
      session: { sessionId, connectionId },
      text: input.text,
      database: d.leaseState?.database ?? d.database,
      leaseId: d.leaseId,
      sourceEditorId: id,
    });
  }, [id, sessionId, connectionId, t, toast, analyzeForRun]);

  const format = () => {
    if (!isSqlDialect(dialect)) return;
    let failed = false;
    editor.current?.replaceSelectionOrDoc((text) => {
      const out = formatSql(text, dialect);
      if (out === null) failed = true;
      return out ?? text;
    });
    if (failed) toast('error', t('format.failed'));
  };

  const saveFile = async (asNew: boolean) => {
    editor.current?.flush();
    const d = useEditorStore.getState().tabs[id];
    if (!d) return;
    try {
      let path: string | null;
      try {
        path = await ipc.sqlFileSave(asNew ? null : d.filePath, d.text);
      } catch (e) {
        // The write grant for the path lives in memory for an hour: after a reload (or later)
        // the backend refuses it. Ask for a new path and keep the text.
        if (asNew || toIpcError(e).kind !== 'permission') throw e;
        path = await ipc.sqlFileSave(null, d.text);
      }
      if (!path) return;
      patchTabData(id, { filePath: path, savedText: d.text });
      setEditorTitle(id, fileBaseName(path));
      toast('success', t('file.saved', { name: fileBaseName(path) }));
    } catch (e) {
      toast('error', t('file.saveFailed', { message: toIpcError(e).message }));
    }
  };

  const openFile = async () => {
    try {
      const file = await ipc.sqlFileOpen();
      if (!file) return;
      // A file always opens in its own tab, on the same connection.
      const tabStateId = crypto.randomUUID();
      addEditorTab(
        { tabStateId, connectionId, sessionId, mode },
        {
          title: fileBaseName(file.path),
          text: file.text,
          cursor: 0,
          filePath: file.path,
          savedText: file.text,
          database: data.leaseState?.database ?? data.database,
          schema: data.leaseState?.schema ?? data.schema,
          autoCommit: data.autoCommit,
        },
      );
    } catch (e) {
      toast('error', t('file.openFailed', { message: toIpcError(e).message }));
    }
  };

  const onShortcut = (action: ShortcutId) => {
    switch (action) {
      case 'focusResults':
        resultsRef.current?.querySelector<HTMLElement>('[role=grid], [role=tab], button')?.focus();
        if (!resultsRef.current?.contains(document.activeElement)) resultsRef.current?.focus();
        return;
      case 'run':
        return void start(
          defaultScope(useEditorStore.getState().tabs[id]?.selection ?? { from: 0, to: 0 }),
        );
      case 'runAll':
        return void start('all');
      case 'explain':
        return isSqlDialect(dialect) ? void explain() : undefined;
      case 'cancel':
        return void cancelRun(id);
      case 'format':
        return format();
      case 'save':
        return void saveFile(false);
      case 'open':
        return void openFile();
      case 'history':
        return setPanel((p) => !p);
    }
  };

  const insert = (text: string, snippet: boolean) => editor.current?.insertText(text, snippet);
  const openInNewTab = (text: string) => {
    addEditorTab(
      { tabStateId: crypto.randomUUID(), connectionId, sessionId, mode },
      {
        title: nextEditorTitle(),
        text,
        cursor: text.length,
        database: data.leaseState?.database ?? data.database,
        schema: data.leaseState?.schema ?? data.schema,
        autoCommit: data.autoCommit,
      },
    );
  };

  const target = useMemo(
    () => ({
      sessionId,
      mode,
      bracketQuotes: dialect.name === 'msSql',
      database: data.leaseState?.database ?? data.database,
      schema: data.leaseState?.schema ?? data.schema,
    }),
    [
      sessionId,
      mode,
      dialect.name,
      data.leaseState?.database,
      data.leaseState?.schema,
      data.database,
      data.schema,
    ],
  );

  const databasePickerShown = mode === 'sql' && !!sessionId && dbPickerAvailable;
  const context = [
    databasePickerShown ? null : (data.leaseState?.database ?? data.database),
    data.leaseState?.schema ?? data.schema,
  ]
    .filter(Boolean)
    .join(' · ');
  const runKeys = formatKeys(keysOf('run'));
  const scope = defaultScope(data.selection);
  const runLabel = t(scope === 'selection' ? 'toolbar.runSelection' : 'toolbar.runStatement');
  const statement =
    data.analyzedText === data.text
      ? statementAt(data.statements, utf16ToCharOffset(data.text, data.selection.from))
      : null;
  const scopeStart =
    scope === 'selection'
      ? data.selection.from
      : statement
        ? charToUtf16Offset(data.text, statement.range.start)
        : null;
  const scopeEnd =
    scope === 'selection'
      ? data.selection.to
      : statement
        ? charToUtf16Offset(data.text, statement.range.end)
        : null;
  const scopeDescription =
    scopeStart !== null && scopeEnd !== null
      ? t('toolbar.scopeLines', {
          from: data.text.slice(0, scopeStart).split('\n').length,
          to: data.text.slice(0, scopeEnd).split('\n').length,
        })
      : t('toolbar.scopePending');

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="editor-tab">
      {openDialog}
      {reconnectDialog}
      <div className="flex min-h-10 shrink-0 flex-wrap items-center gap-3 border-b border-border px-3 py-1">
        <ConnectionPicker tabStateId={id} connectionId={connectionId} mode={mode} />
        {mode === 'sql' && sessionId && (
          <DatabasePicker
            onAvailable={setDbPickerAvailable}
            tabStateId={id}
            sessionId={sessionId}
            defaultDatabase={profile?.database ?? null}
          />
        )}
        {context && (
          <span className="min-w-0 truncate text-[13px] text-muted-foreground" title={context}>
            {context}
          </span>
        )}
      </div>
      <div
        className="flex min-h-10 shrink-0 flex-wrap py-1 items-center gap-2 border-b border-border bg-surface px-2"
        role="toolbar"
        aria-label={t('toolbar.label')}
      >
        <Button
          variant="primary"
          size="sm"
          disabled={!sessionId || running}
          aria-keyshortcuts={keysOf('run')}
          title={`${runLabel} (${runKeys}) · ${scopeDescription}`}
          onClick={() => onShortcut('run')}
        >
          <Play size={14} aria-hidden /> {runLabel}
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={!running || !caps?.cancelQuery}
          onClick={() => onShortcut('cancel')}
          title={formatKeys(keysOf('cancel'))}
        >
          <Square size={13} aria-hidden />
          {t('toolbar.cancel')}
        </Button>
        <DebugButton
          tabStateId={id}
          sessionId={sessionId}
          connectionId={connectionId}
          mode={mode}
        />
        {mode !== 'redis' && (
          <IconButton
            label={`${t('toolbar.runAll')} (${formatKeys(keysOf('runAll'))})`}
            icon={<ListVideo size={14} />}
            disabled={!sessionId || running}
            onClick={() => onShortcut('runAll')}
          />
        )}
        {mode !== 'redis' && (
          <>
            <span className="mx-1 h-4 w-px bg-border" aria-hidden />
            {isSqlDialect(dialect) && (
              <IconButton
                label={`${t('toolbar.explain')} (${formatKeys(keysOf('explain'))})`}
                icon={<ListTree size={14} />}
                disabled={!sessionId || running}
                onClick={() => void explain()}
              />
            )}
            {isSqlDialect(dialect) && (
              <IconButton
                label={`${t('toolbar.format')} (${formatKeys(keysOf('format'))})`}
                icon={<WandSparkles size={14} />}
                onClick={format}
              />
            )}
            <IconButton
              label={`${t('toolbar.open')} (${formatKeys(keysOf('open'))})`}
              icon={<FolderOpen size={14} />}
              onClick={() => void openFile()}
            />
            <IconButton
              label={`${t('toolbar.save')} (${formatKeys(keysOf('save'))})`}
              icon={<Save size={14} />}
              onClick={() => void saveFile(false)}
            />
            <IconButton
              label={t('toolbar.saveAs')}
              icon={<FileDown size={14} />}
              onClick={() => void saveFile(true)}
            />
          </>
        )}
        {data.filePath && (
          <span
            className="ml-1 max-w-48 truncate text-xs text-muted-foreground"
            title={data.filePath}
            data-testid="file-name"
          >
            {fileBaseName(data.filePath)}
            {isDirty(data) && <span aria-label={t('file.unsaved')}> •</span>}
          </span>
        )}
        <span className="ml-auto flex items-center gap-2">
          {data.leaseStatus === 'opening' && <Spinner size={12} />}
          <IconButton
            label={`${t('toolbar.history')} (${formatKeys(keysOf('history'))})`}
            icon={<History size={14} />}
            aria-pressed={panel}
            onClick={() => setPanel((p) => !p)}
          />
          <IconButton
            label={t('toolbar.shortcuts')}
            icon={<CircleHelp size={14} />}
            onClick={() => useShortcutsHelp.getState().setOpen(true)}
          />
        </span>
      </div>

      {mode !== 'redis' && (
        <div
          className="flex min-h-7 shrink-0 items-center gap-2 border-b border-border px-3 text-xs text-muted-foreground"
          data-testid="run-scope"
        >
          <span>
            {runLabel} · {scopeDescription}
          </span>
        </div>
      )}
      {!sessionId && (
        <div
          role="status"
          className="flex items-center gap-2 border-b border-border bg-warning-soft px-2 py-1 text-xs text-warning"
          data-testid="not-connected"
        >
          <PlugZap size={12} aria-hidden />
          <span className="min-w-0 flex-1">
            {t('connection.notConnectedTo', { name: profile?.name ?? '' })}
          </span>
          {connectionId && (
            <Button
              size="sm"
              disabled={opening}
              onClick={() =>
                void openSession(connectionId).catch((e: unknown) => {
                  if (toIpcError(e).kind !== 'cancelled') toast('error', toIpcError(e).message);
                })
              }
            >
              {opening ? <Spinner size={12} /> : null}
              {t('connection.connect')}
            </Button>
          )}
        </div>
      )}
      {data.leaseStatus === 'error' && data.leaseError && (
        <div
          role="alert"
          className="border-b border-border bg-danger-soft px-2 py-1 text-xs text-danger"
        >
          {t('connection.leaseFailed', { message: data.leaseError.message })}
        </div>
      )}
      {showTx && <TxBar data={data} sessionId={sessionId} running={running} />}

      <div className="flex min-h-0 flex-1">
        <div className="min-h-0 min-w-0 flex-1">
          {mode === 'redis' ? (
            <ConsoleView data={data} mode={mode} sessionId={sessionId} />
          ) : (
            <Group orientation="vertical" {...layout}>
              <Panel id="query" defaultSize="55%" minSize="15%">
                <Suspense
                  fallback={
                    <div className="flex h-full items-center justify-center">
                      <Spinner />
                    </div>
                  }
                >
                  <SqlEditor
                    ref={editor}
                    initialText={data.text}
                    initialScroll={{ top: data.scrollTop, left: data.scrollLeft }}
                    onScroll={({ top, left }) =>
                      patchTabData(id, { scrollTop: top, scrollLeft: left })
                    }
                    initialSelection={{ from: data.selection.from, to: data.selection.to }}
                    mode={mode}
                    dialect={dialect}
                    target={target}
                    marks={marks}
                    ariaLabel={t('editor.label')}
                    extraExtensions={debuggerExtensions(id)}
                    onShortcut={onShortcut}
                    onChange={(text) => patchTabData(id, { text })}
                    onSelection={(selection) =>
                      patchTabData(id, { selection, cursor: selection.to })
                    }
                  />
                </Suspense>
              </Panel>
              <Separator className="h-1 bg-border/50 transition-colors hover:bg-accent data-[separator=active]:bg-accent" />
              <Panel id="results" minSize="10%">
                <div
                  ref={resultsRef}
                  tabIndex={-1}
                  className="h-full min-h-0"
                  onKeyDown={(e) => {
                    if (e.key === 'F6') {
                      e.preventDefault();
                      editor.current?.focus();
                    }
                  }}
                >
                  <DebugBottomHost tabStateId={id}>
                    <ResultsPanel
                      tabStateId={id}
                      run={run}
                      mode={mode}
                      onRevealError={
                        errorMark
                          ? () => editor.current?.reveal(errorMark.from, errorMark.to)
                          : undefined
                      }
                      onLoadMore={() => void fetchMore(id)}
                    />
                  </DebugBottomHost>
                </div>
              </Panel>
            </Group>
          )}
        </div>
        {panel && (
          <SidePanel
            connectionId={connectionId}
            dbKind={profile?.kind ?? null}
            runId={run?.id ?? null}
            currentText={() => {
              const d = useEditorStore.getState().tabs[id];
              if (!d) return '';
              return d.selection.from === d.selection.to
                ? d.text
                : d.text.slice(d.selection.from, d.selection.to);
            }}
            onInsert={insert}
            onOpenInNewTab={openInNewTab}
          />
        )}
      </div>

      <ParamsDialog
        open={paramsFor !== null}
        refs={paramsFor?.refs ?? []}
        previous={data.params}
        onCancel={() => setParamsFor(null)}
        onSubmit={(drafts) => {
          const pending = paramsFor;
          setParamsFor(null);
          patchTabData(id, {
            params: [
              ...data.params.filter((p) => !drafts.some((d) => d.name === p.name)),
              ...drafts,
            ],
          });
          pending?.run(drafts.map(toParamValue));
        }}
      />
    </div>
  );
}
