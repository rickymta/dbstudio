// Result tabs of a run (QUERY-05): one tab per statement (grid, affected rows or "done"), plus
// a "Messages" tab with server messages, the error and timings.

import { AlertCircle, CheckCircle2, Loader2, PanelRight, Table2 } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { EmptyState } from '@/components/ui/EmptyState';
import { IconButton } from '@/components/ui/IconButton';
import { Spinner } from '@/components/ui/Spinner';
import { cn } from '@/lib/cn';
import { CellViewer } from '@/features/data/CellViewer';
import { DataGrid } from '@/features/data/DataGrid';
import { useEditSession } from '@/features/data/edit/editSession';
import { useTabUi } from '@/features/data/store';
import { useGridVersion } from '@/features/data/useGridVersion';
import { useAppSettings } from '@/features/settings/queries';
import { describeError } from '@/features/connections/errors';
import type { EditorMode } from '@/stores/tabs';
import { getConsolePlugin } from './consolePlugins';
import { dropPending, runText } from './editorRunner';
import { getTabData, patchTabData, useTabData, type RunSnapshot } from './editorStore';
import { ResultEditBar, ResultSaveBar } from './ResultEditBar';
import { hasOrigins, loadResultEditability, patchResultEdit, useResultEdit } from './resultEdit';
import { resultKind, type StatementResult } from './results';

const LAYOUT_KEY = 'dbstudio.editor.resultLayout';

/** First word(s) of the statement a result belongs to, for the tab label. */
function statementLabel(chars: readonly string[], r: StatementResult): string {
  if (!r.range) return '';
  // Only the head of the statement is needed for the verb.
  const text = chars.slice(r.range.start, Math.min(r.range.end, r.range.start + 40)).join('');
  return /^[\s(]*([\p{L}_.$]+)/u.exec(text)?.[1]?.toUpperCase() ?? '';
}

/** The result shown first: the first one with a grid, else the last one. */
function defaultResultKey(results: readonly StatementResult[]): string | null {
  return (results.find((r) => r.model) ?? results[results.length - 1])?.key ?? null;
}

export interface ResultsPanelProps {
  tabStateId: string;
  onRevealError?: () => void;
  run: RunSnapshot | null;
  onLoadMore: () => void;
  /** Editor mode: MongoDB results can also be shown as documents. */
  mode?: EditorMode;
}

const MESSAGES = 'messages';
/** More result tabs than this are not drawn (a script can have thousands of statements). */
const MAX_RESULT_TABS = 50;

export function ResultsPanel({
  tabStateId,
  run,
  onLoadMore,
  mode,
  onRevealError,
}: ResultsPanelProps) {
  const { t } = useTranslation('editor');
  const [picked, setPicked] = useState<{ runId: number; key: string } | null>(
    () => getTabData(tabStateId)?.selectedResult ?? null,
  );

  // Characters of the executed text, split once per run (statement ranges are scalar offsets).
  const chars = useMemo(() => Array.from(run?.text ?? ''), [run?.text]);
  const results = run?.results ?? [];
  const hasMessages =
    !!run && (run.messages.length > 0 || results.some((r) => r.messages.length > 0) || !!run.error);
  const selected =
    picked &&
    run &&
    picked.runId === run.id &&
    (picked.key === MESSAGES ? hasMessages : results.some((r) => r.key === picked.key))
      ? picked.key
      : // A failed run with nothing to show but its error opens on the messages.
        (defaultResultKey(results) ?? (hasMessages ? MESSAGES : null));

  if (!run) {
    return (
      <div className="flex h-full items-center justify-center" data-testid="results-empty">
        <EmptyState
          icon={<Table2 size={28} />}
          title={t('results.empty')}
          description={t('results.emptyHint')}
        />
      </div>
    );
  }

  const select = (key: string) => {
    const next = { runId: run.id, key };
    setPicked(next);
    patchTabData(tabStateId, { selectedResult: next });
  };
  const shown = results.find((r) => r.key === selected);

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="results-panel">
      {run.error && run.status !== 'cancelled' && (
        <div
          role="status"
          className="flex shrink-0 items-center gap-2 border-b border-border bg-danger-soft px-3 py-1 text-xs text-danger"
        >
          <span className="min-w-0 flex-1 truncate" title={run.error.message}>
            {run.error.message}
          </span>
          {onRevealError && (
            <Button size="sm" onClick={onRevealError}>
              {t('results.goToError')}
            </Button>
          )}
        </div>
      )}
      <div
        role="tablist"
        aria-label={t('results.tabs')}
        className="flex min-h-9 shrink-0 items-end gap-1 overflow-x-auto border-b border-border bg-surface px-2"
      >
        {results.slice(0, MAX_RESULT_TABS).map((r, i) => (
          <ResultTabButton
            key={r.key}
            chars={chars}
            result={r}
            n={i + 1}
            active={selected === r.key}
            onSelect={() => select(r.key)}
          />
        ))}
        {results.length > MAX_RESULT_TABS && (
          <span
            className="shrink-0 px-1 text-xs text-muted-foreground"
            data-testid="results-hidden"
          >
            {t('results.moreHidden', { count: results.length - MAX_RESULT_TABS })}
          </span>
        )}
        {hasMessages && (
          <button
            type="button"
            role="tab"
            aria-selected={selected === MESSAGES}
            onClick={() => select(MESSAGES)}
            className={tabClass(selected === MESSAGES)}
          >
            {t('results.messages')}
            {run.error && <AlertCircle size={12} className="text-danger" aria-hidden />}
          </button>
        )}
        <RunSummary run={run} />
      </div>
      <div className="min-h-0 flex-1" role="tabpanel">
        {selected === MESSAGES ? (
          <MessagesView run={run} />
        ) : shown ? (
          <ResultBody
            tabStateId={tabStateId}
            run={run}
            result={shown}
            onLoadMore={onLoadMore}
            mode={mode}
          />
        ) : run.status === 'running' ? (
          <div className="flex h-full items-center justify-center gap-2 text-muted-foreground">
            <Spinner /> <span>{t('results.running')}</span>
          </div>
        ) : (
          <EmptyState className="h-full" title={t('results.noResults')} />
        )}
      </div>
    </div>
  );
}

const tabClass = (active: boolean) =>
  cn(
    '-mb-px flex h-7 shrink-0 items-center gap-1.5 rounded-t border border-b-0 px-2 text-xs',
    active
      ? 'border-border bg-background text-foreground'
      : 'border-transparent text-muted-foreground hover:text-foreground',
  );

function ResultTabButton({
  chars,
  result,
  n,
  active,
  onSelect,
}: {
  chars: readonly string[];
  result: StatementResult;
  n: number;
  active: boolean;
  onSelect: () => void;
}) {
  const { t } = useTranslation('editor');
  const kind = resultKind(result);
  const verb = statementLabel(chars, result);
  const icon =
    kind === 'error' ? (
      <AlertCircle size={12} className="text-danger" aria-hidden />
    ) : kind === 'running' ? (
      <Loader2 size={12} className="animate-spin" aria-hidden />
    ) : kind === 'rows' ? (
      <Table2 size={12} aria-hidden />
    ) : (
      <CheckCircle2 size={12} className="text-success" aria-hidden />
    );
  return (
    <button
      type="button"
      role="tab"
      aria-selected={active}
      onClick={onSelect}
      className={tabClass(active)}
    >
      {icon}
      {t('results.tabLabel', { n, verb })}
    </button>
  );
}

function RunSummary({ run }: { run: RunSnapshot }) {
  const { t, i18n } = useTranslation('editor');
  const nf = new Intl.NumberFormat(i18n.language);
  const status =
    run.status === 'running'
      ? t('results.running')
      : run.status === 'cancelled'
        ? t('results.cancelled')
        : run.status === 'error'
          ? t('results.failed')
          : t('results.statements', { count: run.results.length });
  return (
    <span
      className="ml-auto flex shrink-0 items-center gap-2 pb-1 text-xs text-muted-foreground"
      data-testid="run-summary"
    >
      {run.status === 'running' && <Spinner size={12} />}
      {status}
      {run.elapsedMs !== null && (
        <span>{t('results.elapsed', { ms: nf.format(run.elapsedMs) })}</span>
      )}
    </span>
  );
}

function ResultBody({
  tabStateId,
  run,
  result,
  onLoadMore,
  mode,
}: {
  tabStateId: string;
  run: RunSnapshot;
  result: StatementResult;
  onLoadMore: () => void;
  mode?: EditorMode;
}) {
  const { t, i18n } = useTranslation('editor');
  const nf = new Intl.NumberFormat(i18n.language);
  const [asDocuments, setAsDocuments] = useState(false);
  const kind = resultKind(result);
  if (result.model && kind !== 'error') {
    const grid = (
      <ResultGrid
        gridId={`${tabStateId}:${run.id}:${result.key}`}
        tabStateId={tabStateId}
        run={run}
        mode={mode}
        result={result}
        canFetchMore={run.canFetchMore && result === run.results[run.results.length - 1]}
        fetching={run.fetchingMore}
        onLoadMore={onLoadMore}
      />
    );
    // A document result (MongoDB) can be read as Extended JSON through the mode's plug-in.
    const plugin = mode === 'mongo' ? getConsolePlugin(mode) : undefined;
    const documents = plugin?.renderResult?.(result);
    if (!documents) return grid;
    // Its table view lists the documents' keys as columns; other results keep the grid.
    const table = plugin?.renderTable?.(result) ?? grid;
    return (
      <div className="flex h-full min-h-0 flex-col">
        <div className="flex h-7 shrink-0 items-center gap-1 border-b border-border px-2">
          <Button size="sm" aria-pressed={!asDocuments} onClick={() => setAsDocuments(false)}>
            {t('results.viewTable')}
          </Button>
          <Button size="sm" aria-pressed={asDocuments} onClick={() => setAsDocuments(true)}>
            {t('results.viewDocuments')}
          </Button>
        </div>
        <div className="min-h-0 flex-1 overflow-auto">{asDocuments ? documents : table}</div>
      </div>
    );
  }
  if (kind === 'error') return <ErrorView run={run} result={result} />;
  if (kind === 'running') {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-muted-foreground">
        <Spinner /> <span>{t('results.running')}</span>
      </div>
    );
  }
  const elapsed =
    result.elapsedMs !== null ? t('results.elapsed', { ms: nf.format(result.elapsedMs) }) : '';
  return (
    <div className="flex h-full flex-col items-center justify-center" data-testid="result-summary">
      <EmptyState
        icon={<CheckCircle2 size={28} className="text-success" />}
        title={
          result.outcome?.kind === 'affected'
            ? t('results.affected', {
                count: result.outcome.rows,
                formatted: nf.format(result.outcome.rows),
              })
            : t('results.done')
        }
        description={elapsed}
      />
      {/* The statement's own server messages (MongoDB `matched: 1, modified: 0`, `insertedIds`)
          belong next to the count: "0 rows affected" alone does not say whether nothing matched. */}
      {result.messages.length > 0 && (
        <ul
          className="-mt-4 flex max-w-full flex-col items-center gap-0.5 px-4 font-mono text-xs text-muted-foreground"
          data-testid="result-summary-messages"
        >
          {result.messages.map((m, i) => (
            <li key={i} className="break-all whitespace-pre-wrap select-text">
              {m}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function ResultGrid({
  gridId,
  tabStateId,
  run,
  mode,
  result,
  canFetchMore,
  fetching,
  onLoadMore,
}: {
  gridId: string;
  tabStateId: string;
  run: RunSnapshot;
  mode?: EditorMode;
  result: StatementResult;
  canFetchMore: boolean;
  fetching: boolean;
  onLoadMore: () => void;
}) {
  const { t, i18n } = useTranslation('editor');
  const nf = new Intl.NumberFormat(i18n.language);
  const model = result.model!;
  const settings = useAppSettings();
  const [ui, patchUi] = useTabUi(gridId);
  useGridVersion(model);
  const tab = useTabData(tabStateId);
  const edit = useResultEdit(gridId);
  const editSession = useEditSession(gridId);

  // Editing: once the result is complete, ask the backend whether (and by which table and key)
  // its rows can be edited. A result without a column that names its table is never asked.
  const sessionId = tab?.leaseSessionId ?? null;
  const ready = model.status === 'ready' && mode === 'sql';
  const known = ready && hasOrigins(model.columns);
  useEffect(() => {
    if (!known || !sessionId) return;
    void loadResultEditability({
      gridId,
      sessionId,
      leaseId: tab?.leaseId ?? null,
      columns: model.columns,
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [known, sessionId, gridId]);
  const leaseId =
    tab?.leaseId && tab.leaseSessionId === sessionId && !tab.leaseState?.broken
      ? tab.leaseId
      : null;
  const value = edit?.status === 'ready' ? edit.value : null;
  const editable = !!value && edit?.reason === null && editSession.editability?.editable === true;
  // Only the statement that produced this result runs again, never the rest of the script
  // (an INSERT or DELETE next to the SELECT must not run twice).
  const rerunInput = (() => {
    if (!result.range) return run.results.length === 1 ? run : null;
    const chars = Array.from(run.text);
    return {
      text: chars.slice(result.range.start, result.range.end).join(''),
      baseChar: run.baseChar + result.range.start,
      params: run.params,
    };
  })();
  const rerun =
    sessionId && rerunInput ? () => void runText(tabStateId, sessionId, rerunInput) : null;
  const readOnlyReason =
    ready && edit?.status === 'ready' && edit.reason
      ? t(`results.edit.reason.${edit.reason}`, {
          columns: (value?.missingKeyColumns ?? []).join(', '),
        })
      : null;

  // A result owns its view state across tab switches, and releases it on disposal.
  useEffect(() => {
    model.viewIds.add(gridId);
  }, [model, gridId]);

  const loaded = model.rows.length;
  const truncated = result.outcome?.kind === 'rows' && result.outcome.truncated;
  const elapsed =
    result.elapsedMs !== null ? t('results.elapsed', { ms: nf.format(result.elapsedMs) }) : '';

  const onApplied = () => {
    // Saving on the tab's connection discarded the cursor "load more" continues.
    if (run.canFetchMore) patchResultEdit(gridId, { needsRerun: true });
    dropPending(tabStateId);
  };

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="result-grid">
      <div
        className={cn(
          'flex min-h-0 flex-1',
          ui.viewerPosition === 'right' ? 'flex-row' : 'flex-col',
        )}
      >
        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          {editable && value && editSession.editability && sessionId && (
            <ResultEditBar
              gridId={gridId}
              model={model}
              sessionId={sessionId}
              leaseId={leaseId}
              manualCommit={tab?.leaseState ? !tab.leaseState.autoCommit : !tab?.autoCommit}
              value={value}
              editability={editSession.editability}
              needsRerun={edit?.needsRerun ?? false}
              onRerun={rerun}
              onApplied={onApplied}
            />
          )}
          <DataGrid
            tabId={gridId}
            model={model}
            layoutKey={LAYOUT_KEY}
            timestamp={settings.timestampDisplay}
          />
        </div>
        {ui.viewerOpen && (
          <CellViewer tabId={gridId} model={model} timestamp={settings.timestampDisplay} />
        )}
      </div>
      <div className="flex min-h-9 flex-wrap py-1 shrink-0 items-center gap-3 border-t border-border bg-surface px-2 text-xs text-muted-foreground">
        {editable && value && editSession.editability && sessionId && (
          <ResultSaveBar
            gridId={gridId}
            model={model}
            sessionId={sessionId}
            leaseId={leaseId}
            value={value}
            editability={editSession.editability}
            onApplied={onApplied}
          />
        )}
        <span>{t('results.rows', { count: loaded, formatted: nf.format(loaded) })}</span>
        {elapsed && <span>{elapsed}</span>}
        {truncated && canFetchMore && (
          <>
            <span>{t('results.truncated', { formatted: nf.format(loaded) })}</span>
            <Button size="sm" disabled={fetching} onClick={onLoadMore}>
              {fetching ? <Spinner size={12} /> : null}
              {t('results.loadMore')}
            </Button>
          </>
        )}
        {truncated && !canFetchMore && (
          <span>{t('results.truncatedFinal', { formatted: nf.format(loaded) })}</span>
        )}
        {readOnlyReason && (
          <span
            role="status"
            data-testid="result-readonly-reason"
            data-reason={edit?.reason ?? undefined}
            className="min-w-0 truncate"
            title={t('results.edit.readOnly', { reason: readOnlyReason })}
          >
            {t('results.edit.readOnly', { reason: readOnlyReason })}
          </span>
        )}
        {edit?.status === 'error' && edit.error && (
          <span role="alert" className="min-w-0 truncate text-danger">
            {t('results.edit.error', { message: edit.error.message })}
          </span>
        )}
        <span className="ml-auto" />
        <IconButton
          label={t('results.cellViewer')}
          icon={<PanelRight size={13} />}
          aria-pressed={ui.viewerOpen}
          onClick={() => patchUi({ viewerOpen: !ui.viewerOpen })}
        />
      </div>
    </div>
  );
}

function ErrorView({ run, result }: { run: RunSnapshot; result: StatementResult }) {
  const { t } = useTranslation('editor');
  const err = result.error!;
  const mark = run.error === err ? run.errorMark : null;
  const cancelled = err.kind === 'cancelled';
  // Drivers may add server details after the first line (MongoDB `errInfo` for error 121):
  // the first line is the error, the rest is shown as preformatted detail.
  const [headline, ...detailLines] = cancelled ? [t('results.cancelled')] : err.message.split('\n');
  const detail = detailLines.join('\n');
  return (
    <div className="h-full overflow-auto p-4" data-testid="result-error" role="alert">
      <div
        className={cn(
          'flex items-start gap-2',
          cancelled ? 'text-muted-foreground' : 'text-danger',
        )}
      >
        <AlertCircle size={16} className="mt-0.5 shrink-0" aria-hidden />
        <div className="min-w-0">
          <p className="font-medium break-words select-text">{headline}</p>
          {detail && (
            <pre
              className="mt-2 font-mono text-xs break-words whitespace-pre-wrap text-foreground select-text"
              data-testid="result-error-detail"
            >
              {detail}
            </pre>
          )}
          {mark && (
            <p className="mt-1 text-xs text-muted-foreground">
              {t('results.errorAt', { line: mark.line, column: mark.column })}
            </p>
          )}
        </div>
      </div>
    </div>
  );
}

function MessagesView({ run }: { run: RunSnapshot }) {
  const { t } = useTranslation('editor');
  const { t: tc } = useTranslation();
  const lines: { key: string; text: string; tone?: 'error' }[] = [];
  run.messages.forEach((m, i) => lines.push({ key: `run${i}`, text: m }));
  run.results.forEach((r) => {
    r.messages.forEach((m, j) => lines.push({ key: `${r.key}-${j}`, text: m }));
    if (r.error && r.error.kind !== 'cancelled') {
      lines.push({ key: `${r.key}-err`, text: describeError(tc, r.error), tone: 'error' });
    }
  });
  return (
    <div className="h-full overflow-auto p-3 font-mono text-xs" data-testid="messages-view">
      {lines.length === 0 ? (
        <p className="text-muted-foreground">{t('results.noMessages')}</p>
      ) : (
        <ul className="flex flex-col gap-1">
          {lines.map((l) => (
            <li
              key={l.key}
              className={cn(
                'break-words whitespace-pre-wrap select-text',
                l.tone === 'error' && 'text-danger',
              )}
            >
              {l.text}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
