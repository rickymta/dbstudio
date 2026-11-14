import { describe, expect, it } from 'vitest';
import type { DebugOutput } from '@/lib/types';
import { buildConsole, filterOf, type OutputItem } from './consoleModel';

const items = (outputs: DebugOutput[]): OutputItem[] =>
  outputs.map((output, i) => ({ id: i + 1, seq: i + 1, output }));

describe('console model', () => {
  it('folds a step with its result set, end and messages', () => {
    const entries = buildConsole(
      items([
        { kind: 'stepStart', step: 1, line: 3, text: 'select 1' },
        {
          kind: 'query',
          step: 1,
          event: { type: 'columns', columns: [{ name: 'a', typeName: 'int4', valueKind: 'int' }] },
        },
        { kind: 'query', step: 1, event: { type: 'rows', rows: [[{ t: 'int', v: 1 }]] } },
        { kind: 'query', step: 1, event: { type: 'rows', rows: [[{ t: 'int', v: 2 }]] } },
        { kind: 'message', text: 'hello', source: 'notice' },
        {
          kind: 'stepEnd',
          step: 1,
          end: { outcome: { kind: 'rows', rowCount: 2, truncated: false }, elapsedMs: 4 },
          truncatedAt: 2,
        },
        { kind: 'error', step: 2, error: { kind: 'query', message: 'boom' } },
      ]),
    );
    expect(entries.map((e) => e.type)).toEqual(['step', 'result', 'message', 'error']);
    const step = entries[0]!;
    expect(step.type === 'step' && step.end?.elapsedMs).toBe(4);
    const result = entries[1]!;
    expect(result.type === 'result' && result.rows).toHaveLength(2);
    expect(result.type === 'result' && result.truncatedAt).toBe(2);
  });

  it('assigns filters', () => {
    const [step, msg, log, err] = buildConsole(
      items([
        { kind: 'stepStart', step: 1, line: 1, text: 'x' },
        { kind: 'message', text: 'n', source: 'dbmsOutput' },
        { kind: 'message', text: 'l', source: 'log' },
        { kind: 'error', error: { kind: 'query', message: 'e' } },
      ]),
    );
    expect([step, msg, log, err].map((e) => filterOf(e!))).toEqual([
      'results',
      'messages',
      'log',
      'errors',
    ]);
  });
});
