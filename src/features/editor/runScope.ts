// Which text a "run" button sends: the selection, the statement at the cursor, or the script.

import { charToUtf16Offset, utf16ToCharOffset } from '@/lib/text';
import type { StatementInfo } from '@/lib/types';

export type RunScope = 'selection' | 'statement' | 'all';

export interface RunText {
  text: string;
  /** Scalar offset of `text` inside the document (error positions are relative to `text`). */
  baseChar: number;
}

/**
 * Statement at the cursor: the one that contains it (a cursor right after the last character
 * counts); in the gap between statements the previous one wins, and before the first the
 * first. `cursorChar` is a scalar offset.
 */
export function statementAt(
  statements: readonly StatementInfo[],
  cursorChar: number,
): StatementInfo | null {
  if (statements.length === 0) return null;
  const inside = statements.find((s) => cursorChar >= s.range.start && cursorChar <= s.range.end);
  if (inside) return inside;
  let previous: StatementInfo | null = null;
  for (const s of statements) if (s.range.end < cursorChar) previous = s;
  return previous ?? statements[0]!;
}

/**
 * Text for `scope`, or `null` when there is nothing to run (blank document, empty selection).
 * Selection and cursor are UTF-16 indexes; statement ranges are scalar offsets.
 */
export function selectRunText(
  doc: string,
  scope: RunScope,
  selection: { from: number; to: number },
  statements: readonly StatementInfo[],
): RunText | null {
  if (scope === 'all') return doc.trim() === '' ? null : { text: doc, baseChar: 0 };
  if (scope === 'selection') {
    const text = doc.slice(selection.from, selection.to);
    return text.trim() === ''
      ? null
      : { text, baseChar: utf16ToCharOffset(doc, selection.from) };
  }
  const found = statementAt(statements, utf16ToCharOffset(doc, selection.from));
  if (!found) return null;
  const from = charToUtf16Offset(doc, found.range.start);
  const to = charToUtf16Offset(doc, found.range.end);
  return { text: doc.slice(from, to), baseChar: found.range.start };
}

/** What the primary run action does: the selection when there is one, else the cursor's statement. */
export function defaultScope(selection: { from: number; to: number }): RunScope {
  return selection.from === selection.to ? 'statement' : 'selection';
}
