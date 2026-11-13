// Console frame for Redis (and optionally Mongo): scrollback of commands and replies, one input
// line with history (ArrowUp / ArrowDown) and command hints. See `consolePlugins.ts` for the
// plug-in points.

import { ChevronRight } from 'lucide-react';
import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Input } from '@/components/ui/Input';
import { valueToText, visibleWhitespace } from '@/features/data/format';
import { cn } from '@/lib/cn';
import type { Uuid, Vocabulary } from '@/lib/types';
import type { EditorMode } from '@/stores/tabs';
import { getVocabulary } from './catalogLoader';
import { getConsolePlugin } from './consolePlugins';
import { runText } from './editorRunner';
import { patchTabData, type ConsoleEntry, type EditorTabData } from './editorStore';
import { resultKind, type StatementResult } from './results';

const HISTORY_LIMIT = 200;
const MAX_LINES = 500;

export interface ConsoleViewProps {
  data: EditorTabData;
  mode: EditorMode;
  sessionId: Uuid | null;
}

let entryCounter = 0;

/** Plain-text lines of a result (rows as tab separated cells), for the default rendering. */
function resultLines(result: StatementResult): string[] {
  if (result.error) return [result.error.message];
  const kind = resultKind(result);
  if (kind === 'affected' && result.outcome?.kind === 'affected')
    return [`(${result.outcome.rows})`];
  const rows = result.model?.rows ?? [];
  if (rows.length === 0) return kind === 'done' ? ['OK'] : ['(empty)'];
  return rows
    .slice(0, MAX_LINES)
    .map((r) => r.map((v) => visibleWhitespace(valueToText(v))).join('\t'));
}

function DefaultResult({ result }: { result: StatementResult }) {
  const lines = resultLines(result);
  return (
    <pre
      className={cn(
        'font-mono text-xs break-words whitespace-pre-wrap select-text',
        result.error && 'text-danger',
      )}
    >
      {lines.join('\n')}
      {result.messages.length > 0 && `\n${result.messages.join('\n')}`}
    </pre>
  );
}

export function ConsoleView({ data, mode, sessionId }: ConsoleViewProps) {
  const { t } = useTranslation('editor');
  const [line, setLine] = useState('');
  const [cursor, setCursor] = useState<number | null>(null);
  const [vocab, setVocab] = useState<Vocabulary | null>(null);
  const logRef = useRef<HTMLDivElement>(null);
  const running = data.run?.status === 'running';

  useEffect(() => {
    let alive = true;
    if (sessionId) void getVocabulary(sessionId).then((v) => alive && setVocab(v));
    return () => {
      alive = false;
    };
  }, [sessionId]);
  useEffect(() => {
    logRef.current?.scrollTo?.({ top: logRef.current.scrollHeight });
  }, [data.console.length]);

  const first = line.trimStart().split(/\s+/)[0] ?? '';
  const hints = useMemo(() => {
    if (!vocab || line.trimStart() !== first || first === '') return [];
    return vocab.commands
      .filter((c) => c.name.toLowerCase().startsWith(first.toLowerCase()))
      .slice(0, 6);
  }, [vocab, line, first]);

  const submit = async () => {
    const command = line.trim();
    if (!command || !sessionId || running) return;
    setLine('');
    setCursor(null);
    patchTabData(data.id, {
      consoleHistory: [command, ...data.consoleHistory.filter((c) => c !== command)].slice(
        0,
        HISTORY_LIMIT,
      ),
    });
    const snap = await runText(data.id, sessionId, { text: command, baseChar: 0 });
    if (!snap) return;
    const entry: ConsoleEntry = { id: ++entryCounter, command, run: snap };
    patchTabData(data.id, { console: [...data.console.slice(-MAX_LINES), entry] });
  };

  const walk = (dir: 1 | -1) => {
    const h = data.consoleHistory;
    if (h.length === 0) return;
    const next = cursor === null ? (dir === 1 ? 0 : null) : cursor + dir;
    if (next === null || next < 0) {
      setCursor(null);
      setLine('');
    } else if (next < h.length) {
      setCursor(next);
      setLine(h[next]!);
    }
  };

  const plugin = getConsolePlugin(mode);
  return (
    <div className="flex h-full min-h-0 flex-col bg-surface" data-testid="console-view">
      <div ref={logRef} className="min-h-0 flex-1 overflow-auto p-3" aria-live="polite">
        {data.console.length === 0 && (
          <p className="text-xs text-muted-foreground">{t('console.empty')}</p>
        )}
        {data.console.map((entry) => (
          <div key={entry.id} className="mb-2" data-testid="console-entry">
            <div className="flex items-center gap-1 font-mono text-xs font-medium">
              <ChevronRight size={12} aria-hidden /> {entry.command}
            </div>
            <div className="pl-4">
              {entry.run.results.map((r) => (
                <div key={r.key}>
                  {(plugin?.renderResult?.(r) as ReactNode) ?? <DefaultResult result={r} />}
                </div>
              ))}
              {entry.run.status === 'cancelled' && (
                <p className="text-xs text-muted-foreground">{t('results.cancelled')}</p>
              )}
            </div>
          </div>
        ))}
      </div>
      {hints.length > 0 && (
        <ul
          className="border-t border-border bg-surface-muted px-3 py-1 text-xs"
          aria-label={t('console.hints')}
        >
          {hints.map((c) => (
            <li key={c.name}>
              <button
                type="button"
                className="font-mono hover:text-accent"
                onClick={() => setLine(`${c.name} `)}
              >
                {c.syntax}
              </button>
              {c.summary && <span className="ml-2 text-muted-foreground">{c.summary}</span>}
            </li>
          ))}
        </ul>
      )}
      <div className="flex items-center gap-2 border-t border-border px-3 py-2">
        <ChevronRight size={14} aria-hidden className="text-muted-foreground" />
        <Input
          autoFocus
          spellCheck={false}
          autoComplete="off"
          className="flex-1 font-mono"
          aria-label={t('console.input')}
          placeholder={sessionId ? t('console.placeholder') : t('connection.notConnected')}
          disabled={!sessionId}
          value={line}
          onChange={(e) => {
            setLine(e.target.value);
            setCursor(null);
          }}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              e.preventDefault();
              void submit();
            } else if (e.key === 'ArrowUp') {
              e.preventDefault();
              walk(1);
            } else if (e.key === 'ArrowDown') {
              e.preventDefault();
              walk(-1);
            }
          }}
        />
      </div>
    </div>
  );
}
