// Types and helpers shared by the ER diagram modules: the persisted layout shape, the view model
// derived from a SchemaModel and the stable keys that tie both together.

import { pathKey } from '@/lib/objectPath';
import type { ColumnDetail, ObjectPath, Relation, SchemaModel, TableModel } from '@/lib/types';

export interface LayoutTable {
  path: ObjectPath;
  x: number;
  y: number;
  collapsed: boolean;
  color?: string;
}

export interface LayoutOptions {
  columns: 'all' | 'keys';
  showTypes: boolean;
  showComments: boolean;
  notation: 'crowsFoot';
  showViews: boolean;
}

export const DEFAULT_OPTIONS: LayoutOptions = {
  columns: 'all',
  showTypes: true,
  showComments: false,
  notation: 'crowsFoot',
  showViews: false,
};

export interface Viewport {
  x: number;
  y: number;
  zoom: number;
}

/** `{ tableKey: [[column, typeName], ...] }` stored with a layout to detect schema changes. */
export type Snapshot = Record<string, [string, string][]>;

export interface Point {
  x: number;
  y: number;
}

export const tableKey = (path: ObjectPath): string => pathKey(path);

const isRecord = (v: unknown): v is Record<string, unknown> =>
  typeof v === 'object' && v !== null && !Array.isArray(v);

const finite = (v: unknown, fallback = 0): number =>
  typeof v === 'number' && Number.isFinite(v) ? v : fallback;

function isPath(v: unknown): v is ObjectPath {
  return (
    isRecord(v) &&
    Array.isArray(v.segments) &&
    v.segments.every((s) => isRecord(s) && typeof s.kind === 'string' && typeof s.name === 'string')
  );
}

/** Lenient parse of the persisted `tables` column; malformed entries are dropped. */
export function parseLayoutTables(raw: unknown): LayoutTable[] {
  if (!Array.isArray(raw)) return [];
  const out: LayoutTable[] = [];
  for (const item of raw) {
    if (!isRecord(item) || !isPath(item.path)) continue;
    out.push({
      path: item.path,
      x: finite(item.x),
      y: finite(item.y),
      collapsed: item.collapsed === true,
      ...(typeof item.color === 'string' ? { color: item.color } : {}),
    });
  }
  return out;
}

/** Lenient parse of the persisted `options` column; missing fields get their defaults. */
export function parseOptions(raw: unknown): LayoutOptions {
  const o = isRecord(raw) ? raw : {};
  return {
    columns: o.columns === 'keys' ? 'keys' : 'all',
    showTypes: typeof o.showTypes === 'boolean' ? o.showTypes : DEFAULT_OPTIONS.showTypes,
    showComments:
      typeof o.showComments === 'boolean' ? o.showComments : DEFAULT_OPTIONS.showComments,
    notation: 'crowsFoot',
    showViews: typeof o.showViews === 'boolean' ? o.showViews : DEFAULT_OPTIONS.showViews,
  };
}

export function parseViewport(raw: unknown): Viewport | null {
  if (!isRecord(raw)) return null;
  const { x, y, zoom } = raw;
  if (
    typeof x === 'number' &&
    typeof y === 'number' &&
    typeof zoom === 'number' &&
    [x, y, zoom].every(Number.isFinite) &&
    zoom > 0
  ) {
    return { x, y, zoom };
  }
  return null;
}

export function parseSnapshot(raw: unknown): Snapshot {
  if (!isRecord(raw)) return {};
  const out: Snapshot = {};
  for (const [k, v] of Object.entries(raw)) {
    if (!Array.isArray(v)) continue;
    out[k] = v
      .filter((c): c is [string, string] => Array.isArray(c) && typeof c[0] === 'string')
      .map((c) => [c[0], typeof c[1] === 'string' ? c[1] : '']);
  }
  return out;
}

export interface ViewColumn {
  detail: ColumnDetail;
  pk: boolean;
  fk: boolean;
  unique: boolean;
}

export interface ViewTable {
  key: string;
  table: TableModel;
  /** Columns shown according to the options (all, or only keys). */
  columns: ViewColumn[];
}

export interface ErdView {
  tables: ViewTable[];
  byKey: Map<string, ViewTable>;
  relations: Relation[];
}

/** Table (+ columns shown) and relation view of a schema model for the given options. */
export function buildView(model: SchemaModel, options: LayoutOptions): ErdView {
  const fkCols = new Map<string, Set<string>>();
  for (const r of model.relations) {
    const k = tableKey(r.fromTable);
    const set = fkCols.get(k) ?? new Set<string>();
    r.fromColumns.forEach((c) => set.add(c));
    fkCols.set(k, set);
  }
  const tables: ViewTable[] = [];
  const byKey = new Map<string, ViewTable>();
  for (const t of model.tables) {
    if (t.kind !== 'table' && !options.showViews) continue;
    const key = tableKey(t.path);
    const fks = fkCols.get(key);
    const uniq = new Set(t.uniqueKeys.flat());
    const columns = t.columns
      .map<ViewColumn>((c) => ({
        detail: c,
        pk: c.primaryKey || t.primaryKey.includes(c.name),
        fk: fks?.has(c.name) ?? false,
        unique: uniq.has(c.name),
      }))
      .filter((c) => options.columns === 'all' || c.pk || c.fk || c.unique);
    const vt = { key, table: t, columns };
    tables.push(vt);
    byKey.set(key, vt);
  }
  const relations = model.relations.filter(
    (r) => byKey.has(tableKey(r.fromTable)) && byKey.has(tableKey(r.toTable)),
  );
  return { tables, byKey, relations };
}

/** Display name of a table: `schema.name` when it has a schema. */
export function tableLabel(t: TableModel): string {
  return t.schema ? `${t.schema}.${t.name}` : t.name;
}

export const relationKey = (r: Relation): string =>
  `${tableKey(r.fromTable)}|${r.name}|${tableKey(r.toTable)}`;
