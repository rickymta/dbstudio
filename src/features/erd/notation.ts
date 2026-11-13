// Crow's foot notation: the symbols drawn at both ends of a relation. Pure geometry, shared by
// the React Flow edge and the SVG export so both draw exactly the same thing.

import type { Relation } from '@/lib/types';

export type EndKind = 'one' | 'zeroOne' | 'zeroMany';

export interface EndSymbol {
  /** SVG path data of the bars / prongs (stroke only). */
  d: string;
  /** Open circle ("zero"), when the end has one. */
  circle: { cx: number; cy: number; r: number } | null;
}

/** Ends of a relation: the `child` end sits at the table holding the FK, `parent` at the target. */
export function relationEnds(r: Pick<Relation, 'fromOptional' | 'oneToOne'>): {
  child: EndKind;
  parent: EndKind;
} {
  return {
    // A nullable FK makes the parent optional ("zero or one"); otherwise exactly one.
    parent: r.fromOptional ? 'zeroOne' : 'one',
    // A 1-1 relation has at most one child, any other has zero or many.
    child: r.oneToOne ? 'zeroOne' : 'zeroMany',
  };
}

const f = (n: number) => String(Math.round(n * 100) / 100);

/**
 * Symbol at the border point `(x, y)`; `(ux, uy)` is the unit vector pointing away from the
 * table along the line.
 */
export function endSymbol(kind: EndKind, x: number, y: number, ux: number, uy: number): EndSymbol {
  const nx = -uy;
  const ny = ux;
  const at = (t: number, s = 0) => `${f(x + ux * t + nx * s)} ${f(y + uy * t + ny * s)}`;
  const bar = (t: number) => `M${at(t, 6)}L${at(t, -6)}`;
  switch (kind) {
    case 'one':
      return { d: `${bar(8)}${bar(13)}`, circle: null };
    case 'zeroOne':
      return { d: bar(8), circle: { cx: x + ux * 16, cy: y + uy * 16, r: 4 } };
    case 'zeroMany':
      return {
        d: `M${at(12)}L${at(0, 6)}M${at(12)}L${at(0, -6)}M${at(12)}L${at(0)}`,
        circle: { cx: x + ux * 18, cy: y + uy * 18, r: 4 },
      };
  }
}
