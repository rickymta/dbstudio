// Pure column layout logic: order, hidden, pinned and widths by column NAME (never by position),
// reconciled with the columns the server returns, and persisted per table.

import { clampColumnWidth } from './format';

export interface ColumnLayout {
  /** Display order of all known column names (hidden ones included). */
  order: string[];
  hidden: string[];
  /** Pinned columns stick to the left, in `order` order. */
  pinned: string[];
  widths: Record<string, number>;
}

export const EMPTY_LAYOUT: ColumnLayout = { order: [], hidden: [], pinned: [], widths: {} };

/** Layout for the current column names: lost columns dropped, new ones appended at the end. */
export function reconcileLayout(saved: ColumnLayout, names: string[]): ColumnLayout {
  const known = new Set(names);
  const order = saved.order.filter((n) => known.has(n));
  const inOrder = new Set(order);
  for (const n of names) if (!inOrder.has(n)) order.push(n);
  const widths: Record<string, number> = {};
  for (const [k, v] of Object.entries(saved.widths)) if (known.has(k)) widths[k] = v;
  return {
    order,
    hidden: saved.hidden.filter((n) => known.has(n)),
    pinned: saved.pinned.filter((n) => known.has(n)),
    widths,
  };
}

export interface VisibleColumns {
  pinned: string[];
  free: string[];
}

/** Visible column names split into the pinned group and the scrolling group, in display order. */
export function visibleColumns(layout: ColumnLayout, names: string[]): VisibleColumns {
  const l = reconcileLayout(layout, names);
  const hidden = new Set(l.hidden);
  const pinned = new Set(l.pinned);
  const visible = l.order.filter((n) => !hidden.has(n));
  return {
    pinned: visible.filter((n) => pinned.has(n)),
    free: visible.filter((n) => !pinned.has(n)),
  };
}

const without = (xs: string[], n: string) => xs.filter((x) => x !== n);

export function toggleHidden(layout: ColumnLayout, name: string): ColumnLayout {
  return layout.hidden.includes(name)
    ? { ...layout, hidden: without(layout.hidden, name) }
    : { ...layout, hidden: [...layout.hidden, name] };
}

export function showAll(layout: ColumnLayout): ColumnLayout {
  return { ...layout, hidden: [] };
}

export function setPinned(layout: ColumnLayout, name: string, pinned: boolean): ColumnLayout {
  const rest = without(layout.pinned, name);
  return { ...layout, pinned: pinned ? [...rest, name] : rest };
}

/** Move `name` before/after `target` in the display order. */
export function moveColumn(
  layout: ColumnLayout,
  name: string,
  target: string,
  placement: 'before' | 'after' = 'before',
): ColumnLayout {
  if (name === target) return layout;
  const order = without(layout.order, name);
  const at = order.indexOf(target);
  if (at < 0) return layout;
  order.splice(placement === 'before' ? at : at + 1, 0, name);
  return { ...layout, order };
}

export function setWidth(layout: ColumnLayout, name: string, width: number): ColumnLayout {
  return { ...layout, widths: { ...layout.widths, [name]: clampColumnWidth(width) } };
}

export function resetLayout(): ColumnLayout {
  return EMPTY_LAYOUT;
}

export function isDefaultLayout(layout: ColumnLayout, names: string[]): boolean {
  const l = reconcileLayout(layout, names);
  return (
    l.hidden.length === 0 &&
    l.pinned.length === 0 &&
    Object.keys(l.widths).length === 0 &&
    l.order.every((n, i) => n === names[i])
  );
}

// -- persistence ----------------------------------------------------------------------------

export function layoutStorageKey(connectionId: string, tablePathKey: string): string {
  return `dbstudio.gridLayout.${connectionId}.${tablePathKey}`;
}

const isStrings = (x: unknown): x is string[] =>
  Array.isArray(x) && x.every((s) => typeof s === 'string');

export function loadLayout(key: string): ColumnLayout {
  try {
    const raw = localStorage.getItem(key);
    if (!raw) return EMPTY_LAYOUT;
    const o = JSON.parse(raw) as Partial<ColumnLayout>;
    const widths: Record<string, number> = {};
    if (o.widths && typeof o.widths === 'object') {
      for (const [k, v] of Object.entries(o.widths)) if (typeof v === 'number') widths[k] = v;
    }
    return {
      order: isStrings(o.order) ? o.order : [],
      hidden: isStrings(o.hidden) ? o.hidden : [],
      pinned: isStrings(o.pinned) ? o.pinned : [],
      widths,
    };
  } catch {
    return EMPTY_LAYOUT;
  }
}

export function saveLayout(key: string, layout: ColumnLayout): void {
  try {
    localStorage.setItem(key, JSON.stringify(layout));
  } catch {
    // storage unavailable: the layout just does not persist
  }
}
