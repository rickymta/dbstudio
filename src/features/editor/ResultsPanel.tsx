// Result tabs of a run (QUERY-05): one tab per statement (grid, affected rows or "done"), plus
// a "Messages" tab with server messages, the error and timings.

import { AlertCircle, CheckCircle2, Loader2, PanelRight, Table2 } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { EmptyState } from '@/components/ui/EmptyState';
import { IconButton } from '@/components/ui/IconButton';
import { Spinner } from '@/components/ui/Spinner';
import { cn } from '@/lib/cn';
import { CellViewer } from '@/features/data/CellViewer';
import { DataGrid } from '@/features/data/DataGrid';
import { useDataUiStore, useTabUi } from '@/features/data/store';
import { useGridVersion } from '@/features/data/useGridVersion';
import { useAppSettings } from '@/features/settings/queries';
import { describeError } from '@/features/connections/errors';
import type { RunSnapshot } from './editorStore';
import { resultKind, type StatementResult } from './results';

const LAYOUT_KEY = 'dbstudio.editor.resultLayout';

/** First word(s) of the statement a result belongs to, for the tab label. */
function statementLabel(run: RunSnapshot, r: StatementResult): string {
  if (!r.range) return '';
  const text = Array.from(run.text).slice(r.range.start, r.range.end).join('');
  return /^[\s(]*([\p{L}_.$]+)/u.exec(text)?.[1]?.toUpperCase() ?? '';
}

/** The result shown first: the first one with a grid, else the last one. */
function defaultResultKey(results: readonly StatementResult[]): string | null {
  return (results.find((r) => r.model) ?? results[results.length - 1])?.key ?? null;
}

export interface ResultsPanelProps {
  tabStateId: string;
  run: RunSnapshot | null;
  onLoadMore: () => void;
}

const MESSAGES = 'messages';

export function ResultsPanel({ tabStateId, run, onLoadMore }: ResultsPanelProps) {
  const { t } = useTranslation('editor');
  const [picked, setPicked] = useState<{ runId: number; key: string } | null>(null);

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

  const select = (key: string) => setPicked({ runId: run.id, key });
  const shown = results.find((r) => r.key === selected);

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="results-panel">
      <div
        role="tablist"
        aria-label={t('results.tabs')}
        className="flex h-8 shrink-0 items-end gap-1 overflow-x-auto border-b border-border bg-surface px-2"
      >
        {results.map((r, i) => (
          <ResultTabButton
            key={r.key}
            run={run}
            result={r}
            n={i + 1}
            active={selected === r.key}
            onSelect={() => select(r.key)}
          />
        ))}
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
          <ResultBody tabStateId={tabStateId} run={run} result={shown} onLoadMore={onLoadMore} />
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
  run,
  result,
  n,
  active,
  onSelect,
}: {
  run: RunSnapshot;
  result: StatementResult;
  n: number;
  active: boolean;
  onSelect: () => void;
}) {
  const { t } = useTranslation('editor');
  const kind = resultKind(result);
  const verb = statementLabel(run, result);
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
}: {
  tabStateId: string;
  run: RunSnapshot;
  result: StatementResult;
  onLoadMore: () => void;
}) {
  const { t, i18n } = useTranslation('editor');
  const nf = new Intl.NumberFormat(i18n.language);
  const kind = resultKind(result);
  if (result.model && kind !== 'error') {
    return (
      <ResultGrid
        gridId={`${tabStateId}:${run.id}:${result.key}`}
        result={result}
        canFetchMore={run.canFetchMore && result === run.results[run.results.length - 1]}
        fetching={run.fetchingMore}
        onLoadMore={onLoadMore}
      />
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
    <div className="flex h-full items-center justify-center" data-testid="result-summary">
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
    </div>
  );
}

function ResultGrid({
  gridId,
  result,
  canFetchMore,
  fetching,
  onLoadMore,
}: {
  gridId: string;
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

  // Drop the UI state of this grid (selection, viewer) with it.
  useEffect(() => () => useDataUiStore.getState().drop(gridId), [gridId]);

  const loaded = model.rows.length;
  const truncated = result.outcome?.kind === 'rows' && result.outcome.truncated;
  const elapsed =
    result.elapsedMs !== null ? t('results.elapsed', { ms: nf.format(result.elapsedMs) }) : '';

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="result-grid">
      <div
        className={cn(
          'flex min-h-0 flex-1',
          ui.viewerPosition === 'right' ? 'flex-row' : 'flex-col',
        )}
      >
        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
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
      <div className="flex h-7 shrink-0 items-center gap-3 border-t border-border bg-surface px-2 text-xs text-muted-foreground">
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
          <p className="font-medium break-words select-text">
            {cancelled ? t('results.cancelled') : err.message}
          </p>
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
