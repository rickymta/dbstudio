// Row storage and fetch orchestration of one table tab, kept outside React.
//
// Rows live in a plain mutable array (`rows`); React only sees a version counter through
// `subscribe`/`getVersion` (useSyncExternalStore). Row batches are coalesced to one notification
// per animation frame, status transitions notify immediately. Every reset bumps a generation:
// events, completions and errors of an older fetch are ignored.

import { ipc } from '@/lib/ipc';
import type { ColumnMeta, IpcError, ObjectPath, SortKey, Uuid, Value } from '@/lib/types';
import {
  EMPTY_APPLIED,
  buildApplied,
  buildCountRequest,
  buildTableRequest,
  nextSort,
  type AppliedFilter,
  type FilterDraft,
} from './request';

/**
 * Rows requested per page, growing with each page so ~1M rows need tens of requests instead of
 * a thousand. The last entry repeats.
 */
export const PAGE_SIZES: readonly number[] = [1_000, 5_000, 20_000];
/** Rows per streamed batch (NFR-PERF-02: 500-1000). */
export const FETCH_BATCH_SIZE = 1_000;
/** Loading stops at this many rows (NFR-PERF-03) ... */
export const MAX_LOADED_ROWS = 1_000_000;
/** ... or when rows x columns reaches this many cells, whichever comes first (memory budget). */
export const MAX_LOADED_CELLS = 20_000_000;
/** After Stop, `queryCancel` is retried this many times when the backend says `notFound`. */
export const CANCEL_RETRIES = 2;
export const CANCEL_RETRY_DELAY_MS = 150;

export type CapReason = 'rows' | 'cells';

export type GridStatus = 'idle' | 'loading' | 'ready' | 'stopped' | 'error';

export interface GridModelOptions {
  sessionId: Uuid;
  table: ObjectPath;
  /** Fixed page size (default: growing `PAGE_SIZES`). */
  pageSize?: number;
  maxRows?: number;
  maxCells?: number;
  /** Called whenever the rows are discarded (reload): selections by row index become invalid. */
  onReset?: () => void;
}

type Scheduler = { request: (cb: () => void) => unknown; cancel: (handle: unknown) => void };

const frameScheduler: Scheduler = {
  request: (cb) =>
    typeof requestAnimationFrame === 'function' ? requestAnimationFrame(cb) : setTimeout(cb, 16),
  cancel: (h) =>
    typeof cancelAnimationFrame === 'function'
      ? cancelAnimationFrame(h as number)
      : clearTimeout(h as ReturnType<typeof setTimeout>),
};

export class GridModel {
  readonly sessionId: Uuid;
  readonly table: ObjectPath;
  readonly pageSizes: readonly number[];
  maxRows: number;
  maxCells: number;

  columns: ColumnMeta[] = [];
  /** Mutable on purpose; read it after `getVersion()` changed. */
  rows: Value[][] = [];
  status: GridStatus = 'idle';
  error: IpcError | null = null;
  /** No more rows to fetch (short page, or the cap was hit). */
  exhausted = false;
  /** Loading stopped because a row or cell budget was reached. */
  capped = false;
  capReason: CapReason | null = null;
  sort: SortKey[] = [];
  applied: AppliedFilter = EMPTY_APPLIED;
  /** Row count from `tableCount` for the applied filter; `null` until requested. */
  total: number | null = null;
  counting = false;
  countError: IpcError | null = null;
  /** Wall time of the most recently finished page fetch. */
  lastFetchMs: number | null = null;

  private version = 0;
  private generation = 0;
  private countGeneration = 0;
  private queryId: Uuid | null = null;
  private pagesLoaded = 0;
  private countQueryId: Uuid | null = null;
  private onReset: (() => void) | undefined;
  private listeners = new Set<() => void>();
  private framePending = false;
  private frameHandle: unknown = null;
  private disposed = false;

  constructor(
    opts: GridModelOptions,
    private scheduler: Scheduler = frameScheduler,
  ) {
    this.sessionId = opts.sessionId;
    this.table = opts.table;
    this.pageSizes = opts.pageSize ? [opts.pageSize] : PAGE_SIZES;
    this.maxRows = opts.maxRows ?? MAX_LOADED_ROWS;
    this.maxCells = opts.maxCells ?? MAX_LOADED_CELLS;
    this.onReset = opts.onReset;
  }

  // -- subscription -------------------------------------------------------------------------

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getVersion = (): number => this.version;

  private notify(): void {
    if (this.framePending) {
      this.scheduler.cancel(this.frameHandle);
      this.framePending = false;
    }
    this.version++;
    for (const l of [...this.listeners]) l();
  }

  private scheduleNotify(): void {
    if (this.framePending || this.disposed) return;
    this.framePending = true;
    this.frameHandle = this.scheduler.request(() => {
      this.framePending = false;
      this.notify();
    });
  }

  // -- loading ------------------------------------------------------------------------------

  /** Start the first load once; later calls are no-ops. */
  ensureLoaded(): void {
    if (this.status === 'idle') this.reload();
  }

  /** Discard rows and load from the first page, cancelling any fetch in flight. */
  reload(): void {
    this.cancelRunning();
    this.cancelCount();
    this.generation++;
    this.rows = [];
    this.pagesLoaded = 0;
    this.exhausted = false;
    this.capped = false;
    this.capReason = null;
    this.error = null;
    this.onReset?.();
    this.fetchPage();
  }

  /**
   * Change the row/cell budget (app settings). Applies to the next page: rows already loaded are
   * never cut, and a model stopped at the old cap resumes if the new budget has room.
   */
  setLimits(maxRows: number, maxCells: number): void {
    if (maxRows === this.maxRows && maxCells === this.maxCells) return;
    this.maxRows = maxRows;
    this.maxCells = maxCells;
    if (this.capped && this.rows.length < this.rowBudget().max) {
      this.capped = false;
      this.capReason = null;
      this.exhausted = false;
      this.notify();
    }
  }

  /** Fetch the next page (no-op while loading, after the end, or after stop/error). */
  loadMore(): void {
    if (this.status !== 'ready' || this.exhausted) return;
    this.fetchPage();
  }

  /** Retry after an error (continues where it stopped) or resume after Stop. */
  retry(): void {
    if (this.status !== 'error' && this.status !== 'stopped') return;
    if (this.rows.length === 0) this.reload();
    else {
      this.error = null;
      this.fetchPage();
    }
  }

  /**
   * Stop the running fetch. From the user's point of view it is cancelled right away: rows that
   * arrive later are ignored, rows already received are kept, and the state is "stopped". The
   * backend cancel is retried when it does not know the query yet.
   */
  stop(): void {
    if (this.status !== 'loading') return;
    this.generation++; // everything still coming from this fetch is ignored
    this.cancelRunning();
    this.status = 'stopped';
    this.notify();
  }

  private cancelRunning(): void {
    const id = this.queryId;
    this.queryId = null;
    if (id) this.cancelQuery(id);
  }

  private cancelQuery(id: Uuid): void {
    const attempt = (left: number) => {
      ipc.queryCancel(this.sessionId, id).catch((err: IpcError) => {
        if (err?.kind === 'notFound' && left > 0 && !this.disposed) {
          setTimeout(() => attempt(left - 1), CANCEL_RETRY_DELAY_MS);
        }
      });
    };
    attempt(CANCEL_RETRIES);
  }

  /** Most rows that may be loaded: the row cap, shrunk by the cell budget once columns are known. */
  private rowBudget(): { max: number; reason: CapReason } {
    const byCells =
      this.columns.length > 0 ? Math.floor(this.maxCells / this.columns.length) : Infinity;
    return byCells < this.maxRows
      ? { max: byCells, reason: 'cells' }
      : { max: this.maxRows, reason: 'rows' };
  }

  private fetchPage(): void {
    const gen = this.generation;
    const queryId = crypto.randomUUID();
    this.queryId = queryId;
    const offset = this.rows.length;
    const budget = this.rowBudget();
    const pageSize = this.pageSizes[Math.min(this.pagesLoaded, this.pageSizes.length - 1)]!;
    const limit = Math.min(pageSize, budget.max - offset);
    if (limit <= 0) {
      this.capped = true;
      this.capReason = budget.reason;
      this.exhausted = true;
      this.status = 'ready';
      this.notify();
      return;
    }
    this.status = 'loading';
    this.notify();

    const request = buildTableRequest({
      queryId,
      table: this.table,
      applied: this.applied,
      sort: this.sort,
      offset,
      limit,
      batchSize: FETCH_BATCH_SIZE,
    });
    const started = performance.now();
    let received = 0;
    let endRows: number | null = null;

    ipc
      .tableFetch(this.sessionId, request, (e) => {
        if (gen !== this.generation) return;
        if (e.type === 'columns') {
          this.columns = e.columns;
          this.scheduleNotify();
        } else if (e.type === 'rows') {
          const rows = this.rows;
          for (const r of e.rows) rows.push(r);
          received += e.rows.length;
          this.scheduleNotify();
        } else if (e.type === 'statementEnd' && e.end.outcome.kind === 'rows') {
          endRows = e.end.outcome.rowCount;
        }
      })
      .then(() => {
        if (gen !== this.generation) return;
        this.queryId = null;
        this.lastFetchMs = Math.round(performance.now() - started);
        this.pagesLoaded++;
        // `ipc.tableFetch` resolves after `statementEnd`, so its row count is authoritative.
        const pageRows = endRows ?? received;
        const after = this.rowBudget(); // columns are known now
        if (this.rows.length >= after.max) {
          this.capped = true;
          this.capReason = after.reason;
          this.exhausted = true;
        } else if (pageRows < limit) {
          this.exhausted = true;
        }
        this.status = 'ready';
        this.notify();
      })
      .catch((err: IpcError) => {
        if (gen !== this.generation) return;
        this.queryId = null;
        this.lastFetchMs = Math.round(performance.now() - started);
        if (err.kind === 'cancelled') {
          this.status = 'stopped';
        } else {
          this.status = 'error';
          this.error = err;
        }
        this.notify();
      });
  }

  // -- sort / filter ------------------------------------------------------------------------

  setSort(sort: SortKey[]): void {
    this.sort = sort;
    this.reload();
  }

  /** Header click: asc -> desc -> off; `additive` (Shift) keeps the other sort columns. */
  toggleSort(column: string, additive: boolean): void {
    this.setSort(nextSort(this.sort, column, additive));
  }

  /** Apply a filter draft (or re-apply the same one: acts as a refresh) and reload. */
  applyFilter(draft: FilterDraft): void {
    this.applied = buildApplied(draft);
    this.resetCount();
    this.reload();
  }

  clearFilter(): void {
    this.applied = EMPTY_APPLIED;
    this.resetCount();
    this.reload();
  }

  // -- count --------------------------------------------------------------------------------

  /** Cancel a running count: back to "not counted", no error; a late result is ignored. */
  cancelCount(): void {
    const id = this.countQueryId;
    this.countQueryId = null;
    if (!this.counting) return;
    this.countGeneration++;
    this.counting = false;
    if (id) this.cancelQuery(id);
    this.notify();
  }

  private resetCount(): void {
    if (this.counting) this.cancelCount();
    this.countGeneration++;
    this.total = null;
    this.counting = false;
    this.countError = null;
  }

  /** Count rows matching the applied filter (never automatic). */
  count(): void {
    if (this.counting) return;
    const gen = ++this.countGeneration;
    const queryId = crypto.randomUUID();
    this.countQueryId = queryId;
    this.counting = true;
    this.countError = null;
    this.notify();
    ipc
      .tableCount(this.sessionId, buildCountRequest(this.table, this.applied, queryId))
      .then((n) => {
        if (gen !== this.countGeneration) return;
        this.countQueryId = null;
        this.total = n;
        this.counting = false;
        this.notify();
      })
      .catch((err: IpcError) => {
        if (gen !== this.countGeneration) return;
        this.countQueryId = null;
        this.counting = false;
        if (err.kind !== 'cancelled') this.countError = err;
        this.notify();
      });
  }

  /** Cancel anything running and drop subscribers (the tab was closed). */
  dispose(): void {
    this.cancelCount();
    this.disposed = true;
    this.cancelRunning();
    this.generation++;
    this.countGeneration++;
    if (this.framePending) this.scheduler.cancel(this.frameHandle);
    this.framePending = false;
    this.listeners.clear();
  }
}
