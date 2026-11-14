import { ChevronRight, Info, Trash2 } from 'lucide-react';
import { useEffect, useMemo, useRef, useState, type FC, type KeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Textarea } from '@/components/ui/Textarea';
import { cn } from '@/lib/cn';
import type { EvalResult, StatementEnd } from '@/lib/types';
import { evaluateExpression, historyOf, pushHistory } from './debugActions';
import { isLive, useDebugStore } from './debugStore';
import { buildConsole, filterOf, type ConsoleEntry, type ConsoleFilter } from './consoleModel';
import { MiniGrid } from './MiniGrid';
import { evalValueText } from './valueText';

const FILTERS: ConsoleFilter[] = ['results', 'messages', 'errors', 'log'];
const EMPTY: never[] = [];

function EndText({ end }: { end: StatementEnd }) {
  const { t } = useTranslation('debugger');
  const o = end.outcome;
  const text =
    o.kind === 'rows'
      ? t('console.rows', { n: o.rowCount })
      : o.kind === 'affected'
        ? t('console.affected', { n: o.rows })
        : t('console.done');
  return (
    <span className="text-muted-foreground">
      {' '}
      · {text} · {end.elapsedMs} ms
    </span>
  );
}

/** Result of an evaluation: scalar text, a grid for rows, red for errors, grey when skipped. */
export const EvalView: FC<{ result: EvalResult }> = ({ result }) => {
  const { t } = useTranslation('debugger');
  const v = result.value;
  if (v.kind === 'rows')
    return (
      <MiniGrid columns={v.result.columns} rows={v.result.rows} truncated={v.result.truncated} />
    );
  if (v.kind === 'error') return <div className="text-danger">{v.error.message}</div>;
  if (v.kind === 'skipped')
    return (
      <div className="text-muted-foreground">{t('watches.skipped', { reason: v.reason })}</div>
    );
  return (
    <div className="font-mono">
      {evalValueText(v)}
      <span className="ml-1 text-xs text-muted-foreground">{`{${v.typeName}}`}</span>
    </div>
  );
};

const Entry: FC<{ entry: ConsoleEntry }> = ({ entry }) => {
  const { t } = useTranslation('debugger');
  switch (entry.type) {
    case 'step':
      return (
        <div className="mt-1 font-mono text-[13px]">
          <span className="text-accent">▶ {entry.line}:</span> <span>{entry.text}</span>
          {entry.end && <EndText end={entry.end} />}
        </div>
      );
    case 'result':
      return (
        <div className="pl-4">
          <MiniGrid
            columns={entry.columns}
            rows={entry.rows}
            truncated={entry.truncatedAt != null}
          />
          {entry.truncatedAt != null && (
            <div className="text-xs text-muted-foreground">
              {t('console.truncatedAt', { n: entry.truncatedAt })}
            </div>
          )}
        </div>
      );
    case 'message':
      return (
        <div
          className={cn(
            'flex items-start gap-1 pl-4 font-mono text-[13px]',
            entry.source === 'log' || entry.source === 'info'
              ? 'text-muted-foreground'
              : entry.source === 'notice'
                ? 'text-warning'
                : '',
          )}
          data-source={entry.source}
        >
          <span className="shrink-0 text-xs text-muted-foreground">
            [{t(`console.source.${entry.source}`)}]
          </span>
          <span className="whitespace-pre-wrap">{entry.text}</span>
        </div>
      );
    case 'error':
      return (
        <div className="pl-4 font-mono text-[13px] whitespace-pre-wrap text-danger" role="alert">
          {entry.error.message}
        </div>
      );
    case 'evaluated':
      return (
        <div className="mt-1">
          <div className="font-mono text-[13px]">
            <span className="text-accent">› </span>
            {entry.expr}
          </div>
          <div className="pl-4">
            <EvalView result={entry.result} />
          </div>
        </div>
      );
  }
};

/** Console tab: the output stream of the session, a filter, and the evaluation prompt. */
export const ConsolePanel: FC<{ tabId: string }> = ({ tabId }) => {
  const { t } = useTranslation('debugger');
  const outputs = useDebugStore((s) => s.sessions[tabId]?.outputs ?? EMPTY);
  const live = useDebugStore((s) => isLive(s.sessions[tabId]));
  const [hidden, setHidden] = useState<Set<ConsoleFilter>>(new Set());
  const [draft, setDraft] = useState('');
  const [cursor, setCursor] = useState<number | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const entries = useMemo(() => buildConsole(outputs), [outputs]);
  const visible = entries.filter((e) => !hidden.has(filterOf(e)));

  useEffect(() => {
    const el = listRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [outputs.length]);

  const toggleFilter = (f: ConsoleFilter) =>
    setHidden((prev) => {
      const next = new Set(prev);
      if (next.has(f)) next.delete(f);
      else next.add(f);
      return next;
    });

  const submit = () => {
    const expr = draft.trim();
    if (!expr || !live) return;
    pushHistory(tabId, expr);
    setDraft('');
    setCursor(null);
    void evaluateExpression(tabId, expr, true);
  };

  const onKey = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      submit();
      return;
    }
    const history = historyOf(tabId);
    if (e.key === 'ArrowUp' && history.length > 0) {
      e.preventDefault();
      const next = cursor === null ? history.length - 1 : Math.max(0, cursor - 1);
      setCursor(next);
      setDraft(history[next]!);
    } else if (e.key === 'ArrowDown' && cursor !== null) {
      e.preventDefault();
      const next = cursor + 1;
      if (next >= history.length) {
        setCursor(null);
        setDraft('');
      } else {
        setCursor(next);
        setDraft(history[next]!);
      }
    }
  };

  return (
    <section className="flex h-full min-h-0 flex-col" aria-label={t('console.title')}>
      <div className="flex shrink-0 items-center gap-1 border-b border-border px-2 py-0.5">
        <span className="mr-1 text-[11px] font-medium text-muted-foreground uppercase">
          {t('console.filter')}
        </span>
        {FILTERS.map((f) => (
          <button
            key={f}
            type="button"
            aria-pressed={!hidden.has(f)}
            className={cn(
              'rounded border px-1.5 py-px text-xs',
              hidden.has(f)
                ? 'border-border text-muted-foreground line-through'
                : 'border-accent/50 bg-accent/10',
            )}
            onClick={() => toggleFilter(f)}
          >
            {t(`console.filters.${f}`)}
          </button>
        ))}
        <span className="flex-1" />
        <Button
          size="sm"
          variant="ghost"
          aria-label={t('console.clear')}
          disabled={outputs.length === 0}
          onClick={() => useDebugStore.getState().patchSession(tabId, { outputs: [] })}
        >
          <Trash2 size={13} aria-hidden /> {t('console.clear')}
        </Button>
      </div>
      <div
        ref={listRef}
        role="log"
        aria-label={t('console.title')}
        className="min-h-0 flex-1 overflow-auto px-2 py-1"
      >
        {visible.length === 0 && (
          <p className="flex items-center gap-1 text-xs text-muted-foreground">
            <Info size={12} aria-hidden /> {t('console.empty')}
          </p>
        )}
        {visible.map((entry) => (
          <Entry key={entry.id} entry={entry} />
        ))}
      </div>
      <div className="flex shrink-0 items-start gap-1 border-t border-border px-2 py-1">
        <ChevronRight size={14} className="mt-1.5 shrink-0 text-accent" aria-hidden />
        <Textarea
          rows={1}
          className="min-h-7 flex-1 resize-none py-1 text-xs"
          value={draft}
          disabled={!live}
          placeholder={live ? t('console.prompt') : t('console.promptEnded')}
          aria-label={t('console.prompt')}
          onChange={(e) => {
            setDraft(e.target.value);
            setCursor(null);
          }}
          onKeyDown={onKey}
        />
      </div>
    </section>
  );
};
