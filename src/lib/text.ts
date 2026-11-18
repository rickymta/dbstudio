// Offsets in the backend contract are Unicode scalar ("char") indexes; JavaScript strings and
// CodeMirror use UTF-16 code units. Astral characters (emoji...) take two units but one char.

/** UTF-16 index of the char at scalar offset `charOffset` (clamped to the text length). */
export function charToUtf16Offset(text: string, charOffset: number): number {
  if (charOffset <= 0) return 0;
  let units = 0;
  let chars = 0;
  while (chars < charOffset && units < text.length) {
    const code = text.codePointAt(units)!;
    units += code > 0xffff ? 2 : 1;
    chars++;
  }
  return units;
}

/** Scalar offset of the UTF-16 index `utf16Offset` (an index inside a pair rounds down). */
export function utf16ToCharOffset(text: string, utf16Offset: number): number {
  const end = Math.min(Math.max(utf16Offset, 0), text.length);
  let chars = 0;
  let units = 0;
  while (units < end) {
    const code = text.codePointAt(units)!;
    const width = code > 0xffff ? 2 : 1;
    if (units + width > end) break;
    units += width;
    chars++;
  }
  return chars;
}

/** Ranges `[start, end)` of every case-insensitive, non-overlapping occurrence of `query`. */
export function matchRanges(text: string, query: string): [number, number][] {
  const q = query.trim().toLowerCase();
  if (!q) return [];
  const lower = text.toLowerCase();
  const ranges: [number, number][] = [];
  let from = 0;
  for (;;) {
    const at = lower.indexOf(q, from);
    if (at < 0) break;
    ranges.push([at, at + q.length]);
    from = at + q.length;
  }
  return ranges;
}
