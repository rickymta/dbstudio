// Mapping of backend error / problem positions (Unicode scalar offsets into the request text)
// to editor ranges (UTF-16 indexes, what CodeMirror uses) and "line, column" labels.

import { charToUtf16Offset } from '@/lib/text';
import type { ScriptProblem, TextRange } from '@/lib/types';

export interface ErrorMark {
  /** UTF-16 range in the document. */
  from: number;
  to: number;
  /** 1-based line and column; the column counts characters (an emoji is one). */
  line: number;
  column: number;
  message: string;
}

/** Line and column (1-based, in characters) of the UTF-16 index `index`. */
export function lineColumn(doc: string, index: number): { line: number; column: number } {
  const end = Math.min(Math.max(index, 0), doc.length);
  let line = 1;
  let lineStart = 0;
  for (let i = 0; i < end; i++) {
    if (doc.charCodeAt(i) === 10) {
      line++;
      lineStart = i + 1;
    }
  }
  // Count scalars, not UTF-16 units, between the line start and the index.
  let column = 1;
  for (let i = lineStart; i < end;) {
    const code = doc.codePointAt(i)!;
    i += code > 0xffff ? 2 : 1;
    column++;
  }
  return { line, column };
}

/** End (UTF-16) of the word or character that starts at `from`, at least one code point long. */
function markEnd(doc: string, from: number): number {
  if (from >= doc.length) return doc.length;
  const m = /^[\p{L}\p{N}_$]+/u.exec(doc.slice(from));
  if (m) return from + m[0].length;
  const code = doc.codePointAt(from)!;
  return from + (code > 0xffff ? 2 : 1);
}

/**
 * Range for an error at scalar offset `position` of the text that was sent, which started at
 * scalar offset `baseChar` of the document. `null` without a position. Positions past the end
 * of the document mark its last character.
 */
export function locateError(
  doc: string,
  baseChar: number,
  position: number | undefined,
  message: string,
): ErrorMark | null {
  if (position === undefined || position < 0) return null;
  let from = charToUtf16Offset(doc, baseChar + position);
  let to = markEnd(doc, from);
  if (from >= doc.length && doc.length > 0) {
    // Past the end: underline the last code point.
    const last = doc.codePointAt(doc.length - 2);
    from = doc.length - (last !== undefined && last > 0xffff ? 2 : 1);
    to = doc.length;
  }
  const { line, column } = lineColumn(doc, from);
  return { from, to, line, column, message };
}

/** Editor ranges of the lexer problems found by `script_analyze` (document-relative offsets). */
export function problemMarks(doc: string, problems: ScriptProblem[]): ErrorMark[] {
  return problems.map((p) => {
    const from = charToUtf16Offset(doc, p.range.start);
    const to = Math.max(charToUtf16Offset(doc, p.range.end), markEnd(doc, from));
    const { line, column } = lineColumn(doc, from);
    return { from, to: Math.min(to, doc.length), line, column, message: p.message };
  });
}

/** UTF-16 range of a statement given by scalar offsets. */
export function statementRange(doc: string, range: TextRange): { from: number; to: number } {
  return { from: charToUtf16Offset(doc, range.start), to: charToUtf16Offset(doc, range.end) };
}
