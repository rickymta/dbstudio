// Pure column layout logic: order, hidden, pinned and widths by column NAME (never by position),
// reconciled with the columns the server returns, and persisted per table.

import { clampColumnWidth } from './format';

export interface ColumnLayout {
  /** Display order of all known column names (hidden ones included). */
  order: string[];
  hidden: string[];
  /** Pinned columns stick to the left, in `order` order. */
  pinned: string[];
  /** Width by column name. Read with `getWidth` (names such as `__proto__` are plain data). */
  widths: Record<string, number>;
}

export const EMPTY_LAYOUT: ColumnLayout = { order: [], hidden: [], pinned: [], widths: {} };

// NOTE: the layout is keyed by column NAME. A result with two columns of the same name (a join
// without aliases) cannot be told apart and would share one slot; that cannot happen for table
// browsing (`table_fetch`), and a future query-result grid must key by position or alias instead.

/** Own-property width lookup: `constructor`, `toString`, `__proto__`... are ordinary names. */
export function getWidth(layout: ColumnLayout, name: string): number | undefined {
  if (!Object.hasOwn(layout.widths, name)) return undefined;
  const w = layout.widths[name];
  return typeof w === 'number' && Number.isFinite(w) ? w : undefined;
}

/** Objects built with `fromEntries` define own properties, so `__proto__` is safe as a key. */
function widthsFrom(entries: [string, number][]): Record<string, number> {
  return Object.fromEntries(entries);
}

/** Layout for the current column names: lost columns dropped, new ones appended at the end. */
export function reconcileLayout(saved: ColumnLayout, names: string[]): ColumnLayout {
  const known = new Set(names);
  const order = [...new Set(saved.order.filter((n) => known.has(n)))];
  const inOrder = new Set(order);
  for (const n of names) if (!inOrder.has(n)) order.push(n);
  return {
    order,
    hidden: saved.hidden.filter((n) => known.has(n)),
    pinned: saved.pinned.filter((n) => known.has(n)),
    widths: widthsFrom(
      Object.entries(saved.widths).filter(([k, v]) => known.has(k) && Number.isFinite(v)),
    ),
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

// Every edit below first reconciles with the current column `names`, so it works on a layout
// that is empty (never saved) or stale (columns added/removed since it was saved).

export function toggleHidden(layout: ColumnLayout, name: string, names: string[]): ColumnLayout {
  const l = reconcileLayout(layout, names);
  if (!names.includes(name)) return l;
  return l.hidden.includes(name)
    ? { ...l, hidden: without(l.hidden, name) }
    : { ...l, hidden: [...l.hidden, name] };
}

export function showAll(layout: ColumnLayout, names: string[]): ColumnLayout {
  return { ...reconcileLayout(layout, names), hidden: [] };
}

export function setPinned(
  layout: ColumnLayout,
  name: string,
  pinned: boolean,
  names: string[],
): ColumnLayout {
  const l = reconcileLayout(layout, names);
  if (!names.includes(name)) return l;
  const rest = without(l.pinned, name);
  return { ...l, pinned: pinned ? [...rest, name] : rest };
}

/** Move `name` before/after `target` in the display order. */
export function moveColumn(
  layout: ColumnLayout,
  name: string,
  target: string,
  names: string[],
  placement: 'before' | 'after' = 'before',
): ColumnLayout {
  const l = reconcileLayout(layout, names);
  if (name === target || !names.includes(name) || !names.includes(target)) return l;
  const order = without(l.order, name);
  order.splice(order.indexOf(target) + (placement === 'before' ? 0 : 1), 0, name);
  return { ...l, order };
}

export function setWidth(
  layout: ColumnLayout,
  name: string,
  width: number,
  names: string[],
): ColumnLayout {
  const l = reconcileLayout(layout, names);
  if (!names.includes(name)) return l;
  const entries = Object.entries(l.widths).filter(([k]) => k !== name);
  return { ...l, widths: widthsFrom([...entries, [name, clampColumnWidth(width)]]) };
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

const MAX_ENTRIES = 2000;
const MAX_NAME_LENGTH = 512;

function cleanNames(x: unknown): string[] {
  if (!Array.isArray(x)) return [];
  const out: string[] = [];
  for (const item of x) {
    if (typeof item === 'string' && item.length <= MAX_NAME_LENGTH) out.push(item);
    if (out.length >= MAX_ENTRIES) break;
  }
  return out;
}

function cleanWidths(x: unknown): Record<string, number> {
  if (typeof x !== 'object' || x === null || Array.isArray(x)) return {};
  const entries: [string, number][] = [];
  for (const [k, v] of Object.entries(x)) {
    if (k.length > MAX_NAME_LENGTH || typeof v !== 'number' || !Number.isFinite(v)) continue;
    entries.push([k, clampColumnWidth(v)]);
    if (entries.length >= MAX_ENTRIES) break;
  }
  return widthsFrom(entries);
}

/** Stored layout, sanitized: unknown shapes are dropped, widths clamped to the allowed range. */
export function loadLayout(key: string): ColumnLayout {
  try {
    const raw = localStorage.getItem(key);
    if (!raw) return EMPTY_LAYOUT;
    const o: unknown = JSON.parse(raw);
    if (typeof o !== 'object' || o === null) return EMPTY_LAYOUT;
    const r = o as Record<string, unknown>;
    return {
      order: cleanNames(r.order),
      hidden: cleanNames(r.hidden),
      pinned: cleanNames(r.pinned),
      widths: cleanWidths(r.widths),
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
