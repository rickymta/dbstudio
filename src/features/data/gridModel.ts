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

/** Rows requested per fetch page. */
export const PAGE_SIZE = 1000;
/** Rows per streamed batch (NFR-PERF-02: 500-1000). */
export const FETCH_BATCH_SIZE = 500;
/** Loading stops here (NFR-PERF-03) and the user is told. */
export const MAX_LOADED_ROWS = 1_000_000;

export type GridStatus = 'idle' | 'loading' | 'ready' | 'stopped' | 'error';

export interface GridModelOptions {
  sessionId: Uuid;
  table: ObjectPath;
  pageSize?: number;
  maxRows?: number;
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
  readonly pageSize: number;
  readonly maxRows: number;

  columns: ColumnMeta[] = [];
  /** Mutable on purpose; read it after `getVersion()` changed. */
  rows: Value[][] = [];
  status: GridStatus = 'idle';
  error: IpcError | null = null;
  /** No more rows to fetch (short page, or the cap was hit). */
  exhausted = false;
  /** Loading stopped because `maxRows` was reached. */
  capped = false;
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
    this.pageSize = opts.pageSize ?? PAGE_SIZE;
    this.maxRows = opts.maxRows ?? MAX_LOADED_ROWS;
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
    this.generation++;
    this.rows = [];
    this.exhausted = false;
    this.capped = false;
    this.error = null;
    this.fetchPage();
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

  /** Ask the backend to cancel the running fetch; already received rows are kept. */
  stop(): void {
    if (this.status === 'loading') this.cancelRunning();
  }

  private cancelRunning(): void {
    const id = this.queryId;
    this.queryId = null;
    if (id) ipc.queryCancel(this.sessionId, id).catch(() => undefined);
  }

  private fetchPage(): void {
    const gen = this.generation;
    const queryId = crypto.randomUUID();
    this.queryId = queryId;
    const offset = this.rows.length;
    const limit = Math.min(this.pageSize, this.maxRows - offset);
    if (limit <= 0) {
      this.capped = true;
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
        }
      })
      .then(() => {
        if (gen !== this.generation) return;
        this.queryId = null;
        this.lastFetchMs = Math.round(performance.now() - started);
        if (this.rows.length >= this.maxRows) {
          this.capped = true;
          this.exhausted = true;
        } else if (received < limit) {
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

  private resetCount(): void {
    this.countGeneration++;
    this.total = null;
    this.counting = false;
    this.countError = null;
  }

  /** Count rows matching the applied filter (never automatic). */
  count(): void {
    if (this.counting) return;
    const gen = ++this.countGeneration;
    this.counting = true;
    this.countError = null;
    this.notify();
    ipc
      .tableCount(this.sessionId, buildCountRequest(this.table, this.applied))
      .then((n) => {
        if (gen !== this.countGeneration) return;
        this.total = n;
        this.counting = false;
        this.notify();
      })
      .catch((err: IpcError) => {
        if (gen !== this.countGeneration) return;
        this.countError = err;
        this.counting = false;
        this.notify();
      });
  }

  /** Cancel anything running and drop subscribers (the tab was closed). */
  dispose(): void {
    this.disposed = true;
    this.cancelRunning();
    this.generation++;
    this.countGeneration++;
    if (this.framePending) this.scheduler.cancel(this.frameHandle);
    this.framePending = false;
    this.listeners.clear();
  }
}
