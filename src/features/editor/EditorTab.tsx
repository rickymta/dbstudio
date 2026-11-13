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
import { useHostKeyPrompt } from '@/features/connections/useHostKeyPrompt';
import { ipc, toIpcError } from '@/lib/ipc';
import type { ParamRef, ParamValue } from '@/lib/types';
import { useIsOpening, useSession, useSessionsStore } from '@/stores/sessions';
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
import { ConsoleView } from './ConsoleView';
import { isSqlDialect } from './dialects';
import { formatSql } from './format';
import { addEditorTab, fileBaseName, nextEditorTitle, setEditorTitle } from './open';
import { ParamsDialog } from './ParamsDialog';
import { toParamValue } from './params';
import { ResultsPanel } from './ResultsPanel';
import { defaultScope, selectRunText, type RunScope } from './runScope';
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
  // An SSH tunnel to an unknown host stops with `sshHostKey`: ask, trust, then connect again.
  const { run: runWithHostKey, dialog: hostKeyDialog } = useHostKeyPrompt();
  const toast = useToastStore.getState().push;
  const editor = useRef<SqlEditorHandle>(null);
  const [panel, setPanel] = useState(false);
  const [paramsFor, setParamsFor] = useState<{
    refs: ParamRef[];
    run: (p: ParamValue[]) => void;
  } | null>(null);

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

  const start = useCallback(
    async (scope: RunScope) => {
      if (!sessionId) {
        toast('info', t('connection.notConnected'));
        return;
      }
      editor.current?.flush();
      const d = useEditorStore.getState().tabs[id];
      if (!d) return;
      const wholeAnalysis = await ipc.scriptAnalyze(sessionId, d.text).catch(() => null);
      const input = selectRunText(d.text, scope, d.selection, wholeAnalysis?.statements ?? []);
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
    [id, sessionId, t, toast],
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
    const whole = await ipc.scriptAnalyze(sessionId, d.text).catch(() => null);
    const input = selectRunText(
      d.text,
      defaultScope(d.selection),
      d.selection,
      whole?.statements ?? [],
    );
    if (!input) {
      toast('info', t('run.nothing'));
      return;
    }
    openPlan({
      session: { sessionId, connectionId },
      text: input.text,
      database: d.leaseState?.database ?? d.database,
      leaseId: d.leaseId,
    });
  }, [id, sessionId, connectionId, t, toast]);

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

  const context = [
    data.leaseState?.database ?? data.database,
    data.leaseState?.schema ?? data.schema,
  ]
    .filter(Boolean)
    .join(' · ');
  const runKeys = formatKeys(keysOf('run'));

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="editor-tab">
      {hostKeyDialog}
      <div
        className="flex h-9 shrink-0 items-center gap-1 border-b border-border bg-surface px-2"
        role="toolbar"
        aria-label={t('toolbar.label')}
      >
        <Button
          variant="primary"
          size="sm"
          disabled={!sessionId || running}
          aria-keyshortcuts={keysOf('run')}
          title={`${t('toolbar.run')} (${runKeys})`}
          onClick={() => onShortcut('run')}
        >
          <Play size={12} aria-hidden /> {t('toolbar.run')}
        </Button>
        {mode !== 'redis' && (
          <IconButton
            label={`${t('toolbar.runAll')} (${formatKeys(keysOf('runAll'))})`}
            icon={<ListVideo size={14} />}
            disabled={!sessionId || running}
            onClick={() => onShortcut('runAll')}
          />
        )}
        {running && (
          <Button
            size="sm"
            variant="danger"
            onClick={() => onShortcut('cancel')}
            title={formatKeys(keysOf('cancel'))}
          >
            <Square size={11} aria-hidden /> {t('toolbar.cancel')}
          </Button>
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
          {context && (
            <span
              className="text-xs text-muted-foreground"
              data-testid="editor-context"
              title={t('toolbar.context')}
            >
              {context}
            </span>
          )}
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
                void runWithHostKey(() => useSessionsStore.getState().open(connectionId)).catch(
                  (e: unknown) => toast('error', toIpcError(e).message),
                )
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
            <Group orientation="vertical">
              <Panel defaultSize="55%" minSize="15%">
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
                    initialSelection={{ from: data.selection.from, to: data.selection.to }}
                    mode={mode}
                    dialect={dialect}
                    target={target}
                    marks={marks}
                    ariaLabel={t('editor.label')}
                    onShortcut={onShortcut}
                    onChange={(text) => patchTabData(id, { text })}
                    onSelection={(selection) =>
                      patchTabData(id, { selection, cursor: selection.to })
                    }
                  />
                </Suspense>
              </Panel>
              <Separator className="h-px bg-border transition-colors hover:bg-accent data-[separator=active]:bg-accent" />
              <Panel minSize="10%">
                <ResultsPanel
                  tabStateId={id}
                  run={run}
                  mode={mode}
                  onLoadMore={() => void fetchMore(id)}
                />
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
