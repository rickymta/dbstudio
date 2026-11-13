// Pure helpers turning UI state (filter draft, sort) into `TableDataRequest` pieces.

import type {
  ColumnFilter,
  FilterOp,
  ObjectPath,
  SortKey,
  TableCountRequest,
  TableDataRequest,
} from '@/lib/types';

export interface FilterRowDraft {
  id: string;
  enabled: boolean;
  column: string;
  op: FilterOp;
  value: string;
}

export interface FilterDraft {
  mode: 'builder' | 'raw';
  rows: FilterRowDraft[];
  raw: string;
}

export interface AppliedFilter {
  filters: ColumnFilter[];
  rawWhere: string | null;
}

export const EMPTY_APPLIED: AppliedFilter = { filters: [], rawWhere: null };

export function emptyDraft(): FilterDraft {
  return { mode: 'builder', rows: [], raw: '' };
}

/** Operators that take no value. */
export function opIsUnary(op: FilterOp): boolean {
  return op === 'isNull' || op === 'isNotNull';
}

/** Values of an `in` list typed as `a, b, c`; blanks are dropped. */
export function parseInList(text: string): string[] {
  return text
    .split(',')
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

/**
 * Filters of the enabled, complete rows. Rows without a column, or with an empty value for an
 * operator that needs one, are skipped (an empty draft row is "not filled in yet").
 */
export function buildFilters(rows: FilterRowDraft[]): ColumnFilter[] {
  const out: ColumnFilter[] = [];
  for (const r of rows) {
    if (!r.enabled || !r.column) continue;
    if (opIsUnary(r.op)) {
      out.push({ column: r.column, op: r.op, values: [] });
    } else if (r.op === 'in') {
      const values = parseInList(r.value);
      if (values.length > 0) out.push({ column: r.column, op: r.op, values });
    } else if (r.value !== '') {
      out.push({ column: r.column, op: r.op, values: [r.value] });
    }
  }
  return out;
}

/** What the draft means to the server: builder rows, or a raw WHERE (never both). */
export function buildApplied(draft: FilterDraft): AppliedFilter {
  if (draft.mode === 'raw') {
    const raw = draft.raw.trim();
    return { filters: [], rawWhere: raw === '' ? null : raw };
  }
  return { filters: buildFilters(draft.rows), rawWhere: null };
}

export function isFilterActive(applied: AppliedFilter): boolean {
  return applied.filters.length > 0 || applied.rawWhere !== null;
}

/**
 * Next sort state after clicking a column header. Plain click: asc -> desc -> off on that
 * column alone. Additive (Shift) click: same cycle for that column while keeping the others;
 * a new column is appended.
 */
export function nextSort(current: SortKey[], column: string, additive: boolean): SortKey[] {
  const existing = current.find((s) => s.column === column);
  const cycled: SortKey | null = !existing
    ? { column, descending: false }
    : existing.descending
      ? null
      : { column, descending: true };
  if (additive) {
    if (!existing) return [...current, { column, descending: false }];
    return current.flatMap((s) => (s.column !== column ? [s] : cycled ? [cycled] : []));
  }
  return cycled ? [cycled] : [];
}

export interface RequestState {
  queryId: string;
  table: ObjectPath;
  applied: AppliedFilter;
  sort: SortKey[];
  offset: number;
  limit: number;
  batchSize?: number;
}

export function buildTableRequest(s: RequestState): TableDataRequest {
  return {
    queryId: s.queryId,
    table: s.table,
    filters: s.applied.filters,
    rawWhere: s.applied.rawWhere,
    sort: s.sort,
    offset: s.offset,
    limit: s.limit,
    batchSize: s.batchSize ?? null,
  };
}

export function buildCountRequest(table: ObjectPath, applied: AppliedFilter): TableCountRequest {
  return { table, filters: applied.filters, rawWhere: applied.rawWhere };
}
