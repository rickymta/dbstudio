import { describe, expect, it } from 'vitest';
import { lineColumn, locateError, problemMarks, statementRange } from './errorMarks';

describe('locateError', () => {
  it('marks the word at the reported position with line and column', () => {
    const doc = 'SELECT 1;\nSELECT nope FROM t';
    // "nope" starts at scalar offset 17 of the document.
    const mark = locateError(doc, 0, 17, 'column "nope" does not exist')!;
    expect(doc.slice(mark.from, mark.to)).toBe('nope');
    expect([mark.line, mark.column]).toEqual([2, 8]);
  });

  it('counts an emoji before the position as one character, not two UTF-16 units', () => {
    const doc = "SELECT '😀😀' AS a, bad_col FROM t";
    const scalarPos = Array.from("SELECT '😀😀' AS a, ").length; // 19 scalars, 21 UTF-16 units
    expect(doc.indexOf('bad_col')).toBe(scalarPos + 2);
    const mark = locateError(doc, 0, scalarPos, 'oops')!;
    expect(doc.slice(mark.from, mark.to)).toBe('bad_col');
    expect(mark.column).toBe(scalarPos + 1);
  });

  it('adds the offset of the executed statement inside the document', () => {
    const doc = "SELECT '🎉';\nSELECT bad FROM t";
    const base = Array.from("SELECT '🎉';\n").length;
    // The statement text sent was "SELECT bad FROM t"; the error is at scalar 7 of it.
    const mark = locateError(doc, base, 7, 'boom')!;
    expect(doc.slice(mark.from, mark.to)).toBe('bad');
    expect([mark.line, mark.column]).toEqual([2, 8]);
  });

  it('marks an emoji itself as one code point, never half a pair', () => {
    const doc = 'SELECT 😀';
    const mark = locateError(doc, 0, 7, 'unexpected')!;
    expect(doc.slice(mark.from, mark.to)).toBe('😀');
  });

  it('marks the last code point for a position past the end and returns null without one', () => {
    const doc = 'SELECT 😀';
    const past = locateError(doc, 0, 99, 'eof')!;
    expect(doc.slice(past.from, past.to)).toBe('😀');
    expect(locateError(doc, 0, undefined, 'x')).toBeNull();
    expect(locateError('', 0, 0, 'x')).toMatchObject({ from: 0, to: 0, line: 1, column: 1 });
  });

  it('computes lines across CRLF and emoji', () => {
    expect(lineColumn('a\r\n😀b', 5)).toEqual({ line: 2, column: 2 });
  });
});

describe('problemMarks / statementRange', () => {
  it('converts lexer problems to UTF-16 ranges', () => {
    const doc = "SELECT '😀' ; SELECT 'open";
    const start = Array.from("SELECT '😀' ; SELECT ").length;
    const marks = problemMarks(doc, [
      { range: { start, end: Array.from(doc).length }, message: 'unterminated string' },
    ]);
    expect(doc.slice(marks[0]!.from, marks[0]!.to)).toBe("'open");
    expect(marks[0]!.message).toBe('unterminated string');
  });

  it('converts statement ranges', () => {
    const doc = '😀; SELECT 1';
    expect(statementRange(doc, { start: 3, end: 11 })).toEqual({ from: 4, to: 12 });
  });
});
