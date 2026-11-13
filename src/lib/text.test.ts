import { describe, expect, it } from 'vitest';
import { charToUtf16Offset, utf16ToCharOffset } from './text';

describe('char <-> UTF-16 offsets', () => {
  it('is the identity for BMP text', () => {
    expect(charToUtf16Offset('select 1', 4)).toBe(4);
    expect(utf16ToCharOffset('select 1', 4)).toBe(4);
    expect(charToUtf16Offset('héllo wörld', 8)).toBe(8);
  });

  it('counts an emoji as one char but two UTF-16 units', () => {
    const text = 'a😀b';
    expect(text.length).toBe(4);
    expect(charToUtf16Offset(text, 0)).toBe(0);
    expect(charToUtf16Offset(text, 1)).toBe(1);
    expect(charToUtf16Offset(text, 2)).toBe(3);
    expect(charToUtf16Offset(text, 3)).toBe(4);
    expect(utf16ToCharOffset(text, 1)).toBe(1);
    expect(utf16ToCharOffset(text, 3)).toBe(2);
    expect(utf16ToCharOffset(text, 4)).toBe(3);
  });

  it('rounds an index inside a surrogate pair down', () => {
    expect(utf16ToCharOffset('a😀b', 2)).toBe(1);
  });

  it('clamps out-of-range offsets', () => {
    expect(charToUtf16Offset('ab', 10)).toBe(2);
    expect(charToUtf16Offset('ab', -3)).toBe(0);
    expect(utf16ToCharOffset('ab', 10)).toBe(2);
    expect(utf16ToCharOffset('ab', -1)).toBe(0);
  });

  it('round-trips every char boundary', () => {
    const text = '😀x\u{1F468}‍\u{1F469}é';
    const chars = Array.from(text).length;
    for (let c = 0; c <= chars; c++) {
      expect(utf16ToCharOffset(text, charToUtf16Offset(text, c))).toBe(c);
    }
  });
});
