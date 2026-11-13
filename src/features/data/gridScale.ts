// Scroll-height scaling for very tall grids.
//
// Browsers cap the height of an element (about 33M px in Blink/WebKit, ~17M px in Gecko), and
// 1,000,000 rows x 24px = 24M px is too close to (or over) that. When the virtual content is
// taller than `MAX_SCROLL_HEIGHT`, the real scroll container is only that tall and the real
// scroll position is mapped linearly onto the virtual one (`virtual = real * ratio`). Rows are
// then placed relative to the viewport (see `rowTop`), so each row keeps its exact pixel height
// and the last row is reachable at the end of the scroll range. With few rows `ratio` is 1 and
// everything degenerates to plain virtual scrolling.

/** Largest scroll content height we ever ask the browser for. */
export const MAX_SCROLL_HEIGHT = 8_000_000;

export interface ScrollScale {
  /** Height of all rows if laid out for real. */
  virtualHeight: number;
  /** Height actually given to the scroll content (<= `MAX_SCROLL_HEIGHT`). */
  realHeight: number;
  /** Virtual pixels per real scroll pixel (>= 1). */
  ratio: number;
}

/**
 * `viewportHeight` is the height of the visible row area (without the sticky header).
 */
export function computeScale(
  rowCount: number,
  rowHeight: number,
  viewportHeight: number,
  maxHeight = MAX_SCROLL_HEIGHT,
): ScrollScale {
  const virtualHeight = rowCount * rowHeight;
  const realHeight = Math.min(virtualHeight, maxHeight);
  if (realHeight >= virtualHeight || realHeight <= viewportHeight) {
    return { virtualHeight, realHeight: virtualHeight, ratio: 1 };
  }
  const ratio = (virtualHeight - viewportHeight) / (realHeight - viewportHeight);
  return { virtualHeight, realHeight, ratio };
}

/** Virtual scroll offset for a real `scrollTop`. */
export function toVirtualOffset(realScrollTop: number, scale: ScrollScale): number {
  return realScrollTop * scale.ratio;
}

/** Position (inside the row area) at which to draw row `index`. */
export function rowTop(
  index: number,
  rowHeight: number,
  realScrollTop: number,
  virtualOffset: number,
): number {
  return Math.round(realScrollTop + index * rowHeight - virtualOffset);
}

/**
 * Real `scrollTop` that brings row `index` fully into view with minimal movement, or `null` when
 * it already is.
 */
export function scrollTopToRevealRow(
  index: number,
  rowHeight: number,
  realScrollTop: number,
  viewportHeight: number,
  scale: ScrollScale,
): number | null {
  const offset = toVirtualOffset(realScrollTop, scale);
  const start = index * rowHeight;
  const end = start + rowHeight;
  let target: number;
  if (start < offset) target = start;
  else if (end > offset + viewportHeight) target = end - viewportHeight;
  else return null;
  return Math.max(0, target / scale.ratio);
}
