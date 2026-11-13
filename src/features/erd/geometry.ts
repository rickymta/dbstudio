// Sizes of table nodes and the geometry of relation edges. Everything is computed from the data,
// never measured from the DOM, so the canvas, the auto layout and the exports agree.

import type { Point, ViewTable, LayoutOptions } from './model';

export const HEADER_HEIGHT = 30;
export const ROW_HEIGHT = 20;
export const NODE_MIN_WIDTH = 170;
export const NODE_MAX_WIDTH = 380;
const CHAR_WIDTH = 7;

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** Visible columns of a node; a collapsed one shows only its header. */
export function visibleRows(t: ViewTable, collapsed: boolean): number {
  return collapsed ? 0 : t.columns.length;
}

export function nodeSize(
  t: ViewTable,
  collapsed: boolean,
  options: Pick<LayoutOptions, 'showTypes'>,
): { width: number; height: number } {
  const title = (t.table.schema ? t.table.schema.length + 1 : 0) + t.table.name.length;
  let widest = title * CHAR_WIDTH + 56;
  if (!collapsed) {
    for (const c of t.columns) {
      const type = options.showTypes ? c.detail.typeName.length + 2 : 0;
      widest = Math.max(widest, (c.detail.name.length + type) * CHAR_WIDTH + 52);
    }
  }
  return {
    width: Math.min(NODE_MAX_WIDTH, Math.max(NODE_MIN_WIDTH, Math.ceil(widest))),
    height: HEADER_HEIGHT + visibleRows(t, collapsed) * ROW_HEIGHT + 4,
  };
}

/** Vertical offset (from the node top) of the row of the first of `columns`, else the middle. */
export function anchorOffset(
  t: ViewTable,
  columns: string[],
  collapsed: boolean,
  height: number,
): number {
  if (!collapsed) {
    const i = t.columns.findIndex((c) => columns.includes(c.detail.name));
    if (i >= 0) return HEADER_HEIGHT + i * ROW_HEIGHT + ROW_HEIGHT / 2;
  }
  return Math.min(height / 2, HEADER_HEIGHT / 2 + 2);
}

export type Side = 'l' | 'r';

export interface EdgeGeometry {
  start: Point;
  end: Point;
  /** Unit vectors pointing away from the tables at the two ends. */
  startDir: Point;
  endDir: Point;
  path: string;
  /** Middle of the edge, for labels. */
  mid: Point;
}

const OVERLAP_GAP = 24;

/** Sides to attach to: facing sides when the rectangles are apart horizontally. */
export function pickSides(from: Rect, to: Rect): [Side, Side] {
  if (from.x + from.width + OVERLAP_GAP <= to.x) return ['r', 'l'];
  if (to.x + to.width + OVERLAP_GAP <= from.x) return ['l', 'r'];
  return ['r', 'r'];
}

const f = (n: number) => String(Math.round(n * 100) / 100);

/**
 * Edge from a row of `from` to a row of `to` (`fromY`/`toY` are absolute y coordinates). The
 * path is a cubic curve leaving and entering horizontally.
 */
export function edgeGeometry(from: Rect, to: Rect, fromY: number, toY: number): EdgeGeometry {
  const [fs, ts] = pickSides(from, to);
  const start = { x: fs === 'r' ? from.x + from.width : from.x, y: fromY };
  const end = { x: ts === 'r' ? to.x + to.width : to.x, y: toY };
  const startDir = { x: fs === 'r' ? 1 : -1, y: 0 };
  const endDir = { x: ts === 'r' ? 1 : -1, y: 0 };
  const dist = Math.hypot(end.x - start.x, end.y - start.y);
  const pull =
    fs === ts ? Math.max(60, Math.abs(end.y - start.y) / 3 + 40) : Math.max(40, dist / 2.5);
  const c1 = { x: start.x + startDir.x * pull, y: start.y };
  const c2 = { x: end.x + endDir.x * pull, y: end.y };
  const path = `M${f(start.x)} ${f(start.y)}C${f(c1.x)} ${f(c1.y)} ${f(c2.x)} ${f(c2.y)} ${f(end.x)} ${f(end.y)}`;
  // Midpoint of the cubic Bezier at t = 0.5.
  const mid = {
    x: (start.x + 3 * c1.x + 3 * c2.x + end.x) / 8,
    y: (start.y + 3 * c1.y + 3 * c2.y + end.y) / 8,
  };
  return { start, end, startDir, endDir, path, mid };
}

/** Bounding box of rectangles (with `pad` around); `null` for none. */
export function boundsOf(rects: Rect[], pad = 0): Rect | null {
  if (rects.length === 0) return null;
  let x0 = Infinity;
  let y0 = Infinity;
  let x1 = -Infinity;
  let y1 = -Infinity;
  for (const r of rects) {
    x0 = Math.min(x0, r.x);
    y0 = Math.min(y0, r.y);
    x1 = Math.max(x1, r.x + r.width);
    y1 = Math.max(y1, r.y + r.height);
  }
  return { x: x0 - pad, y: y0 - pad, width: x1 - x0 + 2 * pad, height: y1 - y0 + 2 * pad };
}
