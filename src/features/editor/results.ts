// Result sets of one editor run. `ResultGridModel` plugs query results into the virtualized grid
// of the data feature (it extends `GridModel`, which only the grid reads); `ResultCollector`
// turns the `QueryEvent` stream of `lease_execute` into one entry per statement.

import { GridModel } from '@/features/data/gridModel';
import type {
  ColumnMeta,
  EndOutcome,
  IpcError,
  QueryEvent,
  TextRange,
  Uuid,
  Value,
} from '@/lib/types';

const NO_TABLE = { segments: [] };

/**
 * A grid model fed by the editor instead of `table_fetch`. Sorting, filtering and counting do
 * not apply (the text of the query decides the order); "load more" continues the pending
 * result of the lease through `onLoadMore`.
 */
export class ResultGridModel extends GridModel {
  private ver = 0;
  private subs = new Set<() => void>();
  private timer: ReturnType<typeof setTimeout> | null = null;
  /** Ask the lease for the next batch of the truncated result; set by the runner. */
  onLoadMore: (() => void) | null = null;

  constructor(sessionId: Uuid) {
    super({ sessionId, table: NO_TABLE });
    this.status = 'loading';
  }

  override subscribe = (listener: () => void): (() => void) => {
    this.subs.add(listener);
    return () => {
      this.subs.delete(listener);
    };
  };

  override getVersion = (): number => this.ver;

  private bump(immediate: boolean): void {
    if (immediate) {
      if (this.timer !== null) clearTimeout(this.timer);
      this.timer = null;
    } else if (this.timer !== null) {
      return;
    } else {
      this.timer = setTimeout(() => {
        this.timer = null;
        this.bump(true);
      }, 16);
      return;
    }
    this.ver++;
    for (const l of [...this.subs]) l();
  }

  setColumns(columns: ColumnMeta[]): void {
    this.columns = columns;
    this.bump(false);
  }

  pushRows(rows: Value[][]): void {
    for (const r of rows) this.rows.push(r);
    this.bump(false);
  }

  /** The statement (or the current fetch batch) ended. */
  finish(truncated: boolean): void {
    this.status = 'ready';
    this.exhausted = !truncated;
    this.error = null;
    this.bump(true);
  }

  /** The next batch was requested. */
  markLoading(): void {
    this.status = 'loading';
    this.bump(true);
  }

  fail(error: IpcError): void {
    this.status = error.kind === 'cancelled' ? 'stopped' : 'error';
    this.error = error;
    this.bump(true);
  }

  override ensureLoaded(): void {}
  override reload(): void {}
  override retry(): void {}
  override stop(): void {}
  override count(): void {}
  override cancelCount(): void {}
  override setSort(): void {}
  override toggleSort(): void {}
  override applyFilter(): void {}
  override clearFilter(): void {}

  override loadMore(): void {
    if (this.status !== 'ready' || this.exhausted) return;
    this.onLoadMore?.();
  }

  override dispose(): void {
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
    this.subs.clear();
    this.onLoadMore = null;
    super.dispose();
  }
}

export interface StatementResult {
  /** Stable key within the run (`r0`, `r1`...). */
  key: string;
  /** Statement index reported by the backend (or the order of arrival). */
  index: number;
  /** Where the statement sits in the request text (scalar offsets), when reported. */
  range: TextRange | null;
  /** Server messages (NOTICE, PRINT, warnings) emitted while the statement ran. */
  messages: string[];
  outcome: EndOutcome | null;
  elapsedMs: number | null;
  /** The statement failed (set by the runner for the statement that raised the error). */
  error: IpcError | null;
  /** Present when the statement produced a result set. */
  model: ResultGridModel | null;
}

export type ResultKind = 'rows' | 'affected' | 'done' | 'error' | 'running';

export function resultKind(r: StatementResult): ResultKind {
  if (r.error) return 'error';
  if (!r.outcome) return 'running';
  return r.outcome.kind;
}

/** Collects the events of one run. Everything is mutated in place; `onChange` fires after each event. */
export class ResultCollector {
  readonly results: StatementResult[] = [];
  /** Messages that arrived outside any statement. */
  readonly runMessages: string[] = [];
  private current: StatementResult | null = null;
  private arrived = 0;

  constructor(
    private readonly sessionId: Uuid,
    private readonly onChange: () => void = () => {},
  ) {}

  private open(index: number, range: TextRange | null): StatementResult {
    const r: StatementResult = {
      key: `r${this.results.length}`,
      index,
      range,
      messages: [],
      outcome: null,
      elapsedMs: null,
      error: null,
      model: null,
    };
    this.results.push(r);
    this.current = r;
    this.arrived++;
    return r;
  }

  private ensureCurrent(): StatementResult {
    if (this.current && this.current.outcome === null) return this.current;
    return this.open(this.arrived, null);
  }

  handle(e: QueryEvent): void {
    switch (e.type) {
      case 'statementStart':
        this.open(e.start.index, e.start.range);
        break;
      case 'columns': {
        const r = this.ensureCurrent();
        // A statement without columns (INSERT...) reports an empty list: no result set.
        if (e.columns.length === 0 && !r.model) break;
        r.model ??= new ResultGridModel(this.sessionId);
        r.model.setColumns(e.columns);
        break;
      }
      case 'rows': {
        const r = this.ensureCurrent();
        r.model ??= new ResultGridModel(this.sessionId);
        r.model.pushRows(e.rows);
        break;
      }
      case 'message':
        if (this.current && this.current.outcome === null) this.current.messages.push(e.message);
        else this.runMessages.push(e.message);
        break;
      case 'statementEnd': {
        const r = this.ensureCurrent();
        r.outcome = e.end.outcome;
        r.elapsedMs = e.end.elapsedMs;
        if (e.end.outcome.kind === 'rows') {
          r.model ??= new ResultGridModel(this.sessionId);
          // After `fetch_more` the event only counts the last batch: report the total.
          r.outcome = { ...e.end.outcome, rowCount: r.model.rows.length };
          r.model.finish(e.end.outcome.truncated);
        } else {
          // A "done" outcome of a statement that streamed columns is still a result set.
          r.model?.finish(false);
        }
        this.current = null;
        break;
      }
    }
    this.onChange();
  }

  /** The run failed: the statement in flight (else a new entry) carries the error. */
  fail(error: IpcError): void {
    const r = this.current && this.current.outcome === null ? this.current : this.open(this.arrived, null);
    r.error = error;
    r.model?.fail(error);
    this.current = null;
    this.onChange();
  }

  /** The statement of the pending (truncated) result continues: events go to it again. */
  resume(r: StatementResult): void {
    this.current = r;
    r.outcome = null;
  }

  /** The last statement whose result set can be continued. */
  get lastResult(): StatementResult | null {
    return this.results[this.results.length - 1] ?? null;
  }

  dispose(): void {
    for (const r of this.results) r.model?.dispose();
  }
}
