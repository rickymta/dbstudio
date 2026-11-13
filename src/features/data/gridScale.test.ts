import { describe, expect, it } from 'vitest';
import {
  MAX_SCROLL_HEIGHT,
  anchoredScrollTop,
  computeScale,
  rowTop,
  scrollTopToRevealRow,
  toVirtualOffset,
} from './gridScale';

const ROW = 24;
const VIEW = 600;

describe('gridScale', () => {
  it('is the identity for small tables', () => {
    const s = computeScale(1000, ROW, VIEW);
    expect(s).toEqual({ virtualHeight: 24_000, realHeight: 24_000, ratio: 1 });
    expect(rowTop(10, ROW, 100, toVirtualOffset(100, s))).toBe(10 * ROW);
  });

  it('caps the real height for ~1M rows and maps the full range', () => {
    const s = computeScale(1_000_000, ROW, VIEW);
    expect(s.virtualHeight).toBe(24_000_000);
    expect(s.realHeight).toBe(MAX_SCROLL_HEIGHT);
    expect(s.ratio).toBeGreaterThan(2.9);

    // At the end of the real scroll range the virtual offset reaches the end of the rows...
    const maxReal = s.realHeight - VIEW;
    expect(toVirtualOffset(maxReal, s)).toBeCloseTo(s.virtualHeight - VIEW, 3);
    // ...so the last row is drawn flush with the bottom of the real content.
    const last = rowTop(999_999, ROW, maxReal, toVirtualOffset(maxReal, s));
    expect(last).toBe(s.realHeight - ROW);
    // The first row sits at the top at scrollTop 0.
    expect(rowTop(0, ROW, 0, 0)).toBe(0);
  });

  it('keeps consecutive rows exactly one row height apart', () => {
    const s = computeScale(1_000_000, ROW, VIEW);
    const real = 3_000_000;
    const vo = toVirtualOffset(real, s);
    expect(rowTop(500_001, ROW, real, vo) - rowTop(500_000, ROW, real, vo)).toBe(ROW);
  });

  it('computes the scroll position that reveals a row', () => {
    const s = computeScale(1_000_000, ROW, VIEW);
    // Already visible: no scroll.
    expect(scrollTopToRevealRow(5, ROW, 0, VIEW, s)).toBeNull();
    // Below the viewport: the row ends up at the bottom edge.
    const down = scrollTopToRevealRow(900_000, ROW, 0, VIEW, s)!;
    expect(toVirtualOffset(down, s) + VIEW).toBeCloseTo(900_001 * ROW, 3);
    // Above the viewport: the row ends up at the top edge.
    const real = 4_000_000;
    const up = scrollTopToRevealRow(10, ROW, real, VIEW, s)!;
    expect(toVirtualOffset(up, s)).toBeCloseTo(10 * ROW, 3);
  });
});

describe('scroll anchoring when the ratio changes', () => {
  it('keeps the first visible row when a page is appended near 400k rows', () => {
    const before = computeScale(400_000, ROW, VIEW);
    expect(before.ratio).toBeGreaterThan(1);
    // The user sits at the very end.
    const realBefore = before.realHeight - VIEW;
    const virtualOffset = toVirtualOffset(realBefore, before);
    const firstRow = Math.floor(virtualOffset / ROW);

    const after = computeScale(405_000, ROW, VIEW);
    expect(after.ratio).not.toBe(before.ratio);

    // Without anchoring the same real scrollTop lands on a different row (the reported jump)...
    const jumped = Math.floor(toVirtualOffset(realBefore, after) / ROW);
    expect(Math.abs(jumped - firstRow)).toBeGreaterThan(500);

    // ...with anchoring the first visible row is unchanged.
    const real = anchoredScrollTop(virtualOffset, after.ratio);
    expect(Math.floor(toVirtualOffset(real, after) / ROW)).toBe(firstRow);
  });
});
