// Turns the raw `debug://output` stream into the entries the Console shows (design 2.9, 5.3):
// one block per step with its result sets, messages, errors and evaluated expressions.

import type {
  ColumnMeta,
  DebugError,
  DebugOutput,
  EvalResult,
  MessageSource,
  StatementEnd,
  Value,
} from '@/lib/types';

export interface OutputItem {
  /** Local, strictly increasing id (React key). */
  id: number;
  seq: number;
  output: DebugOutput;
}

export type ConsoleFilter = 'results' | 'messages' | 'errors' | 'log';

export type ConsoleEntry =
  | { id: number; type: 'step'; step: number; line: number; text: string; end?: StatementEnd }
  | {
      id: number;
      type: 'result';
      step: number;
      columns: ColumnMeta[];
      rows: Value[][];
      /** The engine cut the rows at this count. */
      truncatedAt?: number | null;
    }
  | { id: number; type: 'message'; text: string; source: MessageSource }
  | { id: number; type: 'error'; error: DebugError; step?: number | null }
  | { id: number; type: 'evaluated'; expr: string; result: EvalResult };

/** Which filter chip an entry belongs to. */
export function filterOf(entry: ConsoleEntry): ConsoleFilter {
  switch (entry.type) {
    case 'step':
    case 'result':
    case 'evaluated':
      return 'results';
    case 'error':
      return 'errors';
    case 'message':
      return entry.source === 'log' || entry.source === 'info' ? 'log' : 'messages';
  }
}

/** Fold the output stream into console entries (result sets accumulate rows across events). */
export function buildConsole(outputs: readonly OutputItem[]): ConsoleEntry[] {
  const entries: ConsoleEntry[] = [];
  const stepIndex = new Map<number, number>();
  /** Entry index of the result set being filled, per step. */
  const open = new Map<number, number>();

  for (const { id, output } of outputs) {
    switch (output.kind) {
      case 'stepStart':
        stepIndex.set(output.step, entries.length);
        entries.push({
          id,
          type: 'step',
          step: output.step,
          line: output.line,
          text: output.text,
        });
        break;
      case 'query': {
        const ev = output.event;
        if (ev.type === 'columns') {
          open.set(output.step, entries.length);
          entries.push({ id, type: 'result', step: output.step, columns: ev.columns, rows: [] });
        } else if (ev.type === 'rows') {
          const at = open.get(output.step);
          const target = at === undefined ? undefined : entries[at];
          if (target?.type === 'result') target.rows = target.rows.concat(ev.rows);
        } else if (ev.type === 'message') {
          entries.push({ id, type: 'message', text: ev.message, source: 'notice' });
        }
        // statementStart / statementEnd are covered by stepStart / stepEnd.
        break;
      }
      case 'stepEnd': {
        const at = stepIndex.get(output.step);
        const step = at === undefined ? undefined : entries[at];
        if (step?.type === 'step') step.end = output.end;
        const resultAt = open.get(output.step);
        const result = resultAt === undefined ? undefined : entries[resultAt];
        if (result?.type === 'result') result.truncatedAt = output.truncatedAt ?? null;
        open.delete(output.step);
        break;
      }
      case 'message':
        entries.push({ id, type: 'message', text: output.text, source: output.source });
        break;
      case 'error':
        entries.push({ id, type: 'error', error: output.error, step: output.step });
        break;
      case 'evaluated':
        entries.push({ id, type: 'evaluated', expr: output.expr, result: output.result });
        break;
    }
  }
  return entries;
}
