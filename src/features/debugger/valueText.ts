// Text forms of debugger values (Variables, Watches, hover, Console).

import { valueToText } from '@/features/data/format';
import type { EvalResult, EvalValue, ResultSet } from '@/lib/types';

/** One-line text of an evaluation result. */
export function evalValueText(value: EvalValue): string {
  switch (value.kind) {
    case 'scalar':
      return valueToText(value.value);
    case 'rows': {
      const { rows, columns } = value.result;
      if (rows.length === 1 && columns.length === 1) return valueToText(rows[0]![0]!);
      return `${rows.length} ${rows.length === 1 ? 'row' : 'rows'}`;
    }
    case 'error':
      return value.error.message;
    case 'skipped':
      return value.reason;
  }
}

/** Type shown next to a value (grey, like JetBrains `{numeric}`), when the result has one. */
export function evalTypeName(result: EvalResult): string | null {
  const v = result.value;
  if (v.kind === 'scalar') return v.typeName;
  if (v.kind === 'rows' && v.result.columns.length === 1) return v.result.columns[0]!.typeName;
  return null;
}

/** Rows of a result as tab separated text (header first), for "Copy". */
export function resultToTsv(result: ResultSet): string {
  const clean = (text: string) => text.replace(/[\t\r\n]+/g, ' ');
  const header = result.columns.map((c) => clean(c.name)).join('\t');
  const body = result.rows.map((row) => row.map((v) => clean(valueToText(v))).join('\t'));
  return [header, ...body].join('\n');
}
