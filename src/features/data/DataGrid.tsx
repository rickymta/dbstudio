import { useAppearanceStore, gridRowHeight } from '@/stores/appearance';
import { ArrowDown, ArrowUp } from 'lucide-react';
import {
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  type KeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
} from 'react';
import { observeElementOffset, observeElementRect, useVirtualizer } from '@tanstack/react-virtual';
import { useTranslation } from 'react-i18next';
import type { Value } from '@/lib/types';
import { CellEditorDialog } from './edit/CellEditorDialog';
import { EditableCell } from './edit/EditableCell';
import {
  editDisplayValue,
  editEqualsOriginal,
  isTruncatedValue,
  needsDialogEditor,
} from './edit/cellEdit';
import { patchEditSession, useEditSession } from './edit/editSession';
import {
  changeTargets,
  extractKey,
  rowKeyString,
  usePending,
  usePendingChanges,
  type PendingInsert,
  type PendingUpdate,
} from './edit/pendingChanges';
import { cn } from '@/lib/cn';
import type { EditValue } from '@/lib/types';
import { Spinner } from '@/components/ui/Spinner';
import { ColumnMenu } from './ColumnMenu';
import {
  getWidth,
  moveColumn,
  saveLayout,
  setWidth,
  type ColumnLayout,
  visibleColumns,
} from './columnLayout';
import type { TimestampDisplay } from './timestamp';
import { EmptyState } from '@/components/ui/EmptyState';
import { Button } from '@/components/ui/Button';
import { confirmDialog } from '@/components/ui/confirm';
import {
  defaultColumnWidth,
  formatCell,
  isNumericKind,
  LARGE_COPY_BYTES,
  estimateValueSize,
  valueToText,
  type CellKind,
} from './format';
import {
  computeScale,
  needsOffsetResync,
  reanchorScrollTop,
  rowTop,
  scrollTopToRevealRow,
  virtualOffsetFor,
} from './gridScale';
import type { GridModel } from './gridModel';
import { isFilterActive } from './request';
import { loadScroll, saveScroll, useDataUiStore, useTabUi, type CellPos } from './store';
import { useGridVersion } from './useGridVersion';

export const ROW_HEIGHT = 24;
export const HEADER_HEIGHT = 28;
export const ROWNUM_WIDTH = 56;
/** Start loading the next page when the viewport is this close to the loaded end. */
const LOAD_AHEAD_ROWS = 150;
/** Used when the scroll element reports no size (jsdom / detached): renders a small window. */
const FALLBACK_RECT = { width: 1000, height: 600 };

const TONE: Record<CellKind, string> = {
  null: 'italic text-muted-foreground',
  bool: 'text-accent',
  number: '',
  temporal: '',
  text: '',
  uuid: 'text-muted-foreground',
  binary: 'text-muted-foreground',
  structured: 'text-muted-foreground',
};

/**
 * A 0x0 rect means the element is hidden (tab switch, minimized window): keep the last real size
 * so the scroll scale does not change twice for nothing. Before any real size, use the fallback.
 */
function validRect(
  r: { width: number; height: number },
  last: { current: { width: number; height: number } | null },
) {
  if (r.width === 0 && r.height === 0) return last.current ?? FALLBACK_RECT;
  last.current = r;
  return r;
}

/** Pending state of the row being drawn. */
interface RowContext {
  insert?: PendingInsert;
  update?: PendingUpdate;
  deleted: boolean;
}

interface DataGridProps {
  tabId: string;
  model: GridModel;
  /** localStorage key of this table's column layout. */
  layoutKey: string;
  timestamp?: TimestampDisplay;
}

/**
 * Virtualized read-only grid: rows and columns are virtualized with @tanstack/react-virtual.
 * Row positions come from `gridScale` so ~1M rows still scroll to the end (see that module).
 */
export function DataGrid({ tabId, model, layoutKey, timestamp = 'utc' }: DataGridProps) {
  const appearance = useAppearanceStore((s) => s.appearance);
  const ROW_HEIGHT = gridRowHeight(appearance);
  const HEADER_HEIGHT = Math.max(32, appearance.gridFontSize + 14);
  const { t } = useTranslation('data');
  useGridVersion(model);
  const [ui, patchUi] = useTabUi(tabId);
  const scrollRef = useRef<HTMLDivElement>(null);
  const ratioRef = useRef(1);
  // Ratio / max scrollTop the DOM was last laid out with (null until the grid first mounts).
  const appliedRef = useRef<{ ratio: number; maxTop: number } | null>(null);
  // Re-reports the current scrollTop to the row virtualizer (see the layout effect below).
  const offsetEmitRef = useRef<(() => void) | null>(null);
  const lastRectRef = useRef<{ width: number; height: number } | null>(null);
  const gridId = useId();
  const cellId = (r: number, c: number) => `${gridId}-r${r}c${c}`;

  const { columns, rows } = model;
  const loadedCount = rows.length;

  // Edit mode: pending changes overlay the loaded rows; new rows are shown after them.
  const pending = usePending(tabId);
  const session = useEditSession(tabId);
  const editability = session.editability;
  const editMode = session.editMode && editability?.editable === true;
  const keyColumns = editability?.keyColumns;
  const readOnlyColumns = editability?.readOnlyColumns;
  const inserts = pending.inserts;
  const insertCount = inserts.length;
  // New rows are shown at the top, so loading more pages never moves them.
  const rowCount = insertCount + loadedCount;
  const insertAt = (i: number): PendingInsert | undefined =>
    i < insertCount ? inserts[i] : undefined;
  const loadedAt = (i: number): Value[] | undefined =>
    i >= insertCount ? rows[i - insertCount] : undefined;
  const hasKeyed =
    Object.keys(pending.updates).length > 0 || Object.keys(pending.deletes).length > 0;
  const failedTarget = useMemo(() => {
    if (!session.failure) return null;
    return changeTargets(pending)[session.failure.index] ?? null;
  }, [session.failure, pending]);

  // Column layout is by NAME; everything below maps to real column indexes (selection, copy,
  // sort and the viewer keep using the real index / name, never the display position).
  const names = useMemo(() => columns.map((c) => c.name), [columns]);
  const layout = ui.layout;
  const vis = useMemo(() => visibleColumns(layout, names), [layout, names]);
  const pinnedIdx = useMemo(() => vis.pinned.map((n) => names.indexOf(n)), [vis, names]);
  const freeIdx = useMemo(() => vis.free.map((n) => names.indexOf(n)), [vis, names]);
  const displayOrder = useMemo(() => [...pinnedIdx, ...freeIdx], [pinnedIdx, freeIdx]);
  const widthOf = useCallback(
    (ci: number) => {
      const c = columns[ci];
      return c ? (getWidth(layout, c.name) ?? defaultColumnWidth(c)) : 120;
    },
    [columns, layout],
  );
  const pinnedWidths = pinnedIdx.map(widthOf);
  const pinnedWidth = pinnedWidths.reduce((a, b) => a + b, 0);
  const freeWidths = freeIdx.map(widthOf);

  const commitLayout = useCallback(
    (next: ColumnLayout, persist = true) => {
      patchUi({ layout: next });
      if (persist) saveLayout(layoutKey, next);
    },
    [patchUi, layoutKey],
  );

  // eslint-disable-next-line react-hooks/incompatible-library
  const rowVirtualizer = useVirtualizer({
    count: rowCount,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 12,
    initialRect: FALLBACK_RECT,
    observeElementRect: (inst, cb) =>
      observeElementRect(inst, (r) => cb(validRect(r, lastRectRef))),
    // Real scrollTop -> virtual offset (identity unless the content height is scaled).
    observeElementOffset: (inst, cb) => {
      offsetEmitRef.current = () => {
        const el = inst.scrollElement;
        if (el) cb(virtualOffsetFor(el.scrollTop, ratioRef.current), false);
      };
      const stop = observeElementOffset(inst, (offset, scrolling) =>
        cb(offset * ratioRef.current, scrolling),
      );
      return () => {
        offsetEmitRef.current = null;
        stop?.();
      };
    },
  });

  const colVirtualizer = useVirtualizer({
    horizontal: true,
    count: freeIdx.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: (i) => freeWidths[i] ?? 120,
    getItemKey: (i) => columns[freeIdx[i]!]?.name ?? i,
    overscan: 2,
    paddingStart: ROWNUM_WIDTH + pinnedWidth,
    scrollPaddingStart: ROWNUM_WIDTH + pinnedWidth,
    initialRect: FALLBACK_RECT,
    observeElementRect: (inst, cb) =>
      observeElementRect(inst, (r) => cb(validRect(r, lastRectRef))),
  });

  useEffect(() => {
    rowVirtualizer.measure();
  }, [ROW_HEIGHT, rowVirtualizer]);

  const viewportHeight = rowVirtualizer.scrollRect?.height ?? FALLBACK_RECT.height;
  const bodyViewport = Math.max(ROW_HEIGHT, viewportHeight - HEADER_HEIGHT);
  const scale = computeScale(rowCount, ROW_HEIGHT, bodyViewport);

  // When the scroll ratio or the content/viewport size changes (pages appended, window resized,
  // hidden/shown), keep the first visible row, or the bottom if the view was at the bottom. The
  // position is read from the DOM (`el.scrollTop`), never from the virtualizer's cached offset,
  // which can lag behind the last scroll event.
  useLayoutEffect(() => {
    const el = scrollRef.current;
    const prev = appliedRef.current;
    ratioRef.current = scale.ratio;
    if (!el || el.clientHeight === 0) return; // not laid out (hidden): keep the last state
    const maxTop = Math.max(0, el.scrollHeight - el.clientHeight);
    const applied = { ratio: scale.ratio, maxTop };
    appliedRef.current = applied;
    if (!prev) return;
    if (prev.ratio !== scale.ratio) {
      const next = reanchorScrollTop({
        realTop: el.scrollTop,
        prevRatio: prev.ratio,
        nextRatio: scale.ratio,
        prevMaxRealTop: prev.maxTop,
        maxRealTop: maxTop,
      });
      if (Math.abs(next - el.scrollTop) >= 1) el.scrollTop = next;
    }
    // Assigning an unchanged scrollTop fires no scroll event, so tell the virtualizer explicitly
    // (ratioRef is already updated above): otherwise it keeps the old virtual offset and draws
    // and loads for stale rows.
    if (needsOffsetResync(prev, applied)) offsetEmitRef.current?.();
  }, [scale.ratio, scale.realHeight, viewportHeight]);

  const widthsKey = `${freeWidths.join(',')}|${freeIdx.join(',')}|${pinnedWidth}`;
  useEffect(() => {
    colVirtualizer.measure();
  }, [widthsKey, colVirtualizer]);

  // Restore the scroll position after a remount (tab switch, sub-tab switch).
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const { top, left } = loadScroll(tabId);
    el.scrollTop = top;
    el.scrollLeft = left;
  }, [tabId]);

  const onScroll = useCallback(() => {
    const el = scrollRef.current;
    if (el) saveScroll(tabId, el.scrollTop, el.scrollLeft);
  }, [tabId]);

  const vRows = rowVirtualizer.getVirtualItems();
  const vCols = colVirtualizer.getVirtualItems();
  const realTop = rowVirtualizer.scrollElement?.scrollTop ?? 0;
  // Both come from the DOM scrollTop so rows stay consistent right after a programmatic scroll.
  const virtualOffset = realTop * scale.ratio;
  const lastVisible = vRows.length > 0 ? vRows[vRows.length - 1]!.index : -1;

  // Fetch the next page when the user gets close to the end of what is loaded.
  const status = model.status;
  useEffect(() => {
    if (lastVisible >= 0 && lastVisible >= rowCount - LOAD_AHEAD_ROWS) model.loadMore();
  }, [lastVisible, rowCount, status, model]);

  const totalWidth = colVirtualizer.getTotalSize();
  const selection = ui.selection;
  const activeCellId =
    selection &&
    vRows.some((r) => r.index === selection.row) &&
    (pinnedIdx.includes(selection.col) || vCols.some((c) => freeIdx[c.index] === selection.col))
      ? cellId(selection.row, selection.col)
      : undefined;

  // -- selection ------------------------------------------------------------------------------

  const select = useCallback(
    (pos: CellPos, reveal: boolean) => {
      patchUi({ selection: pos });
      if (!reveal) return;
      const el = scrollRef.current;
      if (!el) return;
      const top = scrollTopToRevealRow(pos.row, ROW_HEIGHT, el.scrollTop, bodyViewport, scale);
      if (top !== null && typeof el.scrollTo === 'function') el.scrollTo({ top });
      const free = freeIdx.indexOf(pos.col);
      if (free >= 0) colVirtualizer.scrollToIndex(free);
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [bodyViewport, scale.ratio, scale.virtualHeight, colVirtualizer, freeIdx],
  );

  const copySelected = useCallback(async () => {
    if (!selection) return;
    const value = rows[selection.row - insertCount]?.[selection.col];
    if (!value || selection.row < insertCount) return;
    if (
      estimateValueSize(value) > LARGE_COPY_BYTES &&
      !(await confirmDialog({
        title: t('common:confirmDialog.largeValueTitle'),
        description: t('grid.copyLarge'),
        confirmLabel: t('common:actions.copy'),
      }))
    ) {
      return;
    }
    void navigator.clipboard?.writeText(valueToText(value, timestamp)).catch(() => undefined);
  }, [rows, insertCount, selection, t, timestamp]);

  // -- editing --------------------------------------------------------------------------------

  const startEdit = (pos: CellPos): boolean => {
    const name = columns[pos.col]?.name;
    if (!editMode || session.applying || !name || readOnlyColumns?.includes(name)) return false;
    if (pos.row >= insertCount) {
      const row = loadedAt(pos.row);
      if (!row || !extractKey(row, columns, keyColumns ?? [])) return false;
      const cell = row[pos.col];
      if (cell && isTruncatedValue(cell)) return false;
    }
    patchEditSession(tabId, { editing: pos });
    return true;
  };

  const stopEdit = () => {
    patchEditSession(tabId, { editing: null });
    scrollRef.current?.focus({ preventScroll: true });
  };

  const commitEdit = (pos: CellPos, edit: EditValue) => {
    const col = columns[pos.col];
    if (!col) return;
    const store = usePendingChanges.getState();
    const ins = insertAt(pos.row);
    if (ins) {
      store.setInsertCell(tabId, ins.id, col.name, edit);
    } else {
      const row = loadedAt(pos.row);
      const key = row ? extractKey(row, columns, keyColumns ?? []) : null;
      const original = row?.[pos.col];
      if (key && original) {
        const same = editEqualsOriginal(original, edit);
        store.setUpdateCell(tabId, key, col.name, same ? null : edit);
      }
    }
    patchEditSession(tabId, { editing: null, failure: null });
    scrollRef.current?.focus({ preventScroll: true });
  };

  const deleteSelectedRow = () => {
    if (!selection) return;
    const store = usePendingChanges.getState();
    if (session.applying) return;
    const ins = insertAt(selection.row);
    if (ins) {
      store.removeInsert(tabId, ins.id);
      patchUi({ selection: null });
      return;
    }
    const row = loadedAt(selection.row);
    const key = row ? extractKey(row, columns, keyColumns ?? []) : null;
    if (key) store.toggleDelete(tabId, key);
  };

  // A new row scrolls into view.
  const insertCountRef = useRef(inserts.length);
  useEffect(() => {
    if (inserts.length > insertCountRef.current) {
      select({ row: 0, col: selection?.col ?? displayOrder[0] ?? 0 }, true);
    }
    insertCountRef.current = inserts.length;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [inserts.length]);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (session.editing) return; // the cell editor owns the keyboard
    const nCols = displayOrder.length;
    if (rowCount === 0 || nCols === 0) return;
    if (editMode && selection) {
      if (e.key === 'F2' || e.key === 'Enter') {
        e.preventDefault();
        if (!startEdit(selection)) patchUi({ viewerOpen: true });
        return;
      }
      if (e.key === 'Delete') {
        e.preventDefault();
        deleteSelectedRow();
        return;
      }
    }
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'c') {
      e.preventDefault();
      void copySelected();
      return;
    }
    if (!selection) {
      if (
        [
          'ArrowUp',
          'ArrowDown',
          'ArrowLeft',
          'ArrowRight',
          'Home',
          'End',
          'PageUp',
          'PageDown',
        ].includes(e.key)
      ) {
        e.preventDefault();
        select({ row: 0, col: displayOrder[0]! }, true);
      }
      return;
    }
    const page = Math.max(1, Math.floor(bodyViewport / ROW_HEIGHT) - 1);
    const ctrl = e.ctrlKey || e.metaKey;
    let { row } = selection;
    // Left/right/Home/End walk the display order (pinned first), not the real column order.
    let pos = Math.max(0, displayOrder.indexOf(selection.col));
    switch (e.key) {
      case 'ArrowUp':
        row -= 1;
        break;
      case 'ArrowDown':
        row += 1;
        break;
      case 'ArrowLeft':
        pos -= 1;
        break;
      case 'ArrowRight':
        pos += 1;
        break;
      case 'PageUp':
        row -= page;
        break;
      case 'PageDown':
        row += page;
        break;
      case 'Home':
        pos = 0;
        if (ctrl) row = 0;
        break;
      case 'End':
        pos = nCols - 1;
        if (ctrl) row = rowCount - 1;
        break;
      case 'Enter':
      case ' ':
        e.preventDefault();
        patchUi({ viewerOpen: true });
        return;
      default:
        return;
    }
    e.preventDefault();
    select(
      {
        row: Math.min(rowCount - 1, Math.max(0, row)),
        col: displayOrder[Math.min(nCols - 1, Math.max(0, pos))]!,
      },
      true,
    );
  };

  const cellFromEvent = (e: ReactMouseEvent): CellPos | null => {
    const target = (e.target as HTMLElement).closest<HTMLElement>('[data-c]');
    if (!target) return null;
    return { row: Number(target.dataset.r), col: Number(target.dataset.c) };
  };

  // -- column resize --------------------------------------------------------------------------

  const dragName = useRef<string | null>(null);
  const startResize = (e: ReactPointerEvent, name: string, startWidth: number) => {
    e.preventDefault();
    e.stopPropagation();
    const startX = e.clientX;
    const latest = () => useDataUiStore.getState().byTab[tabId]?.layout ?? layout;
    const move = (ev: PointerEvent) => {
      commitLayout(setWidth(latest(), name, startWidth + ev.clientX - startX, names), false);
    };
    const up = () => {
      saveLayout(layoutKey, latest());
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  };

  // -- states without a grid ------------------------------------------------------------------

  // With known columns the header stays up while a reload (sort/filter/refresh) is in flight.
  const reloading = rowCount === 0 && columns.length > 0 && model.status === 'loading';
  if (rowCount === 0 && !reloading) {
    if (model.status === 'idle' || model.status === 'loading') {
      return (
        <div className="flex min-h-0 flex-1 items-center justify-center gap-2 text-muted-foreground">
          <Spinner /> <span>{t('grid.loading')}</span>
        </div>
      );
    }
    if (model.status === 'error' && model.error) {
      return (
        <EmptyState
          className="min-h-0 flex-1"
          title={t('grid.errorTitle')}
          description={model.error.message}
          action={
            <Button size="sm" onClick={() => model.retry()}>
              {t('grid.retry')}
            </Button>
          }
        />
      );
    }
    if (model.status === 'stopped') {
      return <EmptyState className="min-h-0 flex-1" title={t('grid.stopped')} />;
    }
    const filtered = isFilterActive(model.applied);
    return (
      <EmptyState
        className="min-h-0 flex-1"
        title={filtered ? t('grid.emptyFiltered') : t('grid.empty')}
        description={filtered ? t('grid.emptyFilteredHint') : undefined}
      />
    );
  }

  const renderHeaderCell = (ci: number, left: number, width: number, key: React.Key) => {
    const col = columns[ci];
    if (!col) return null;
    const sortIndex = model.sort.findIndex((x) => x.column === col.name);
    const sortKey = sortIndex >= 0 ? model.sort[sortIndex]! : null;
    return (
      <div
        key={key}
        role="columnheader"
        draggable
        aria-sort={sortKey ? (sortKey.descending ? 'descending' : 'ascending') : undefined}
        title={`${col.name} (${col.typeName})\n${t('grid.sortHint')}`}
        onClick={(e) => model.toggleSort(col.name, e.shiftKey)}
        onDragStart={(e) => {
          dragName.current = col.name;
          e.dataTransfer.effectAllowed = 'move';
          e.dataTransfer.setData('text/plain', col.name);
        }}
        onDragOver={(e) => {
          if (dragName.current) e.preventDefault();
        }}
        onDrop={(e) => {
          e.preventDefault();
          const from = dragName.current;
          dragName.current = null;
          if (!from || from === col.name) return;
          const rect = e.currentTarget.getBoundingClientRect();
          const after = rect.width > 0 && e.clientX > rect.left + rect.width / 2;
          commitLayout(moveColumn(layout, from, col.name, names, after ? 'after' : 'before'));
        }}
        onDragEnd={() => {
          dragName.current = null;
        }}
        className="absolute top-0 flex cursor-pointer items-center gap-1 border-r border-border px-2 hover:bg-border/50"
        style={{ left, width, height: HEADER_HEIGHT }}
      >
        <span
          className={cn(
            'min-w-0 flex-1 truncate font-medium',
            isNumericKind(col.valueKind) && 'text-right',
          )}
        >
          {col.name}
        </span>
        {sortKey && (
          <span className="flex shrink-0 items-center text-accent">
            {sortKey.descending ? <ArrowDown size={12} /> : <ArrowUp size={12} />}
            {model.sort.length > 1 && (
              <span className="text-[10px] leading-none">{sortIndex + 1}</span>
            )}
          </span>
        )}
        <span
          role="separator"
          aria-label={t('grid.resize')}
          draggable={false}
          className="absolute top-0 right-0 h-full w-1.5 cursor-col-resize hover:bg-accent/40"
          onClick={(e) => e.stopPropagation()}
          onMouseDown={(e) => e.preventDefault()}
          onPointerDown={(e) => startResize(e, col.name, width)}
        />
      </div>
    );
  };

  const renderCell = (
    r: number,
    row: Value[] | undefined,
    ci: number,
    left: number,
    width: number,
    rowSelected: boolean,
    key: React.Key,
    ctx: RowContext,
  ) => {
    const col = columns[ci];
    if (!col) return null;
    const edit: EditValue | undefined = ctx.insert?.cells[col.name] ?? ctx.update?.cells[col.name];
    const editing = session.editing?.row === r && session.editing.col === ci;
    let value = row?.[ci];
    let isDefault = false;
    if (edit) {
      const shown = editDisplayValue(edit, col.valueKind);
      if (shown) value = shown;
      else isDefault = true;
    } else if (!row) {
      isDefault = true; // a new row's cell nobody typed in: the column default applies
    }
    if (!value && !isDefault) return null;
    const d = isDefault
      ? { text: t('edit.default'), align: 'left' as const, kind: 'null' as const }
      : formatCell(value!, undefined, timestamp);
    const selected = rowSelected && selection?.col === ci;
    const current: Value | EditValue | null = edit ?? row?.[ci] ?? null;
    const dialog = editing && needsDialogEditor(col, current);
    const pos = { row: r, col: ci };
    return (
      <div
        key={key}
        id={cellId(r, ci)}
        role="gridcell"
        aria-selected={selected || undefined}
        data-r={r}
        data-c={ci}
        data-pending={edit ? (ctx.insert ? 'inserted' : 'updated') : undefined}
        title={
          editMode && readOnlyColumns?.includes(col.name)
            ? (session.readOnlyHints[col.name] ?? t('edit.generatedColumn'))
            : undefined
        }
        className={cn(
          'absolute top-0 truncate border-r border-b border-border/35 px-2 [unicode-bidi:isolate]',
          d.align === 'right' && 'text-right',
          TONE[d.kind],
          edit &&
            !ctx.insert &&
            'bg-cell-edited font-medium text-cell-edited-foreground not-italic',
          ctx.deleted && 'text-muted-foreground line-through',
          selected && 'z-[5] bg-accent/15 not-italic outline-2 -outline-offset-2 outline-accent',
          editing && !dialog && 'z-[6] overflow-visible p-0',
        )}
        style={{ left, width, height: ROW_HEIGHT, lineHeight: `${ROW_HEIGHT - 1}px` }}
      >
        {editing && !dialog ? (
          <EditableCell
            column={col}
            current={current}
            allowDefault={!!ctx.insert}
            onCommit={(e) => commitEdit(pos, e)}
            onCancel={stopEdit}
          />
        ) : (
          d.text
        )}
        {dialog && (
          <CellEditorDialog
            column={col}
            current={current}
            onCommit={(e) => commitEdit(pos, e)}
            onCancel={stopEdit}
          />
        )}
      </div>
    );
  };

  return (
    <div className="relative min-h-0 flex-1">
      <div
        ref={scrollRef}
        role="grid"
        aria-label={t('grid.ariaLabel')}
        aria-rowcount={rowCount + 1}
        aria-activedescendant={activeCellId}
        aria-colcount={columns.length}
        tabIndex={0}
        onKeyDown={onKeyDown}
        onScroll={onScroll}
        className="absolute inset-0 select-none overflow-auto bg-surface grid-typography outline-none"
      >
        <div
          className="relative"
          style={{ width: totalWidth, height: HEADER_HEIGHT + scale.realHeight }}
        >
          {/* Sticky header */}
          <div
            role="row"
            aria-rowindex={1}
            className="sticky top-0 z-20 flex border-b border-border bg-surface-muted"
            style={{ width: totalWidth, height: HEADER_HEIGHT }}
          >
            <div
              className="sticky left-0 z-30 flex shrink-0 items-center justify-center border-r border-border bg-surface-muted text-muted-foreground"
              style={{ width: ROWNUM_WIDTH, height: HEADER_HEIGHT, position: 'sticky' }}
            >
              <ColumnMenu
                columns={columns}
                layout={layout}
                onChange={(next) => commitLayout(next)}
              />
            </div>
            {pinnedIdx.length > 0 && (
              <div
                className="sticky z-[25] shrink-0 border-r-2 border-border bg-surface-muted"
                style={{ left: ROWNUM_WIDTH, width: pinnedWidth, height: HEADER_HEIGHT }}
              >
                {pinnedIdx.map((ci, i) =>
                  renderHeaderCell(
                    ci,
                    pinnedWidths.slice(0, i).reduce((a, b) => a + b, 0),
                    pinnedWidths[i]!,
                    `p${ci}`,
                  ),
                )}
              </div>
            )}
            {vCols.map((vc) => {
              const ci = freeIdx[vc.index];
              return ci === undefined ? null : renderHeaderCell(ci, vc.start, vc.size, vc.key);
            })}
          </div>

          {/* Body */}
          <div
            className="absolute left-0 font-mono"
            style={{ top: HEADER_HEIGHT, width: totalWidth, height: scale.realHeight }}
            onMouseDown={(e) => {
              const pos = cellFromEvent(e);
              if (pos) select(pos, false);
            }}
            onDoubleClick={(e) => {
              const pos = cellFromEvent(e);
              if (pos) {
                select(pos, false);
                if (editMode && startEdit(pos)) return;
                patchUi({ selection: pos, viewerOpen: true });
              }
            }}
          >
            {vRows.map((vr) => {
              const insert = insertAt(vr.index);
              const row = loadedAt(vr.index);
              if (!row && !insert) return null;
              const top = rowTop(vr.index, ROW_HEIGHT, realTop, virtualOffset);
              const rowSelected = selection?.row === vr.index;
              let update: PendingUpdate | undefined;
              let deleted = false;
              let rowKey: string | null = null;
              if (row && hasKeyed && keyColumns) {
                const key = extractKey(row, columns, keyColumns);
                if (key) {
                  rowKey = rowKeyString(key);
                  update = pending.updates[rowKey];
                  deleted = rowKey in pending.deletes;
                }
              }
              const ctx: RowContext = { insert, update, deleted };
              const rowState = insert
                ? 'inserted'
                : deleted
                  ? 'deleted'
                  : update
                    ? 'updated'
                    : null;
              const failed =
                failedTarget !== null &&
                (failedTarget.kind === 'insert'
                  ? insert?.id === failedTarget.id
                  : rowKey === failedTarget.rowKey);
              return (
                <div
                  key={vr.index}
                  role="row"
                  aria-rowindex={vr.index + 2}
                  data-pending={rowState ?? undefined}
                  data-failed={failed || undefined}
                  className={cn(
                    'group absolute left-0 flex hover:bg-accent/5',
                    rowState === 'inserted' && 'bg-success-soft',
                    rowState === 'deleted' && 'bg-danger-soft',
                    failed && 'outline-2 -outline-offset-2 outline-danger',
                  )}
                  style={{ top, height: ROW_HEIGHT, width: totalWidth }}
                >
                  <div
                    className={cn(
                      'sticky left-0 z-10 flex shrink-0 items-center justify-end border-r border-b border-border bg-surface-muted px-2 text-[11px] text-muted-foreground',
                      rowSelected && 'text-foreground',
                    )}
                    style={{
                      width: ROWNUM_WIDTH,
                      height: ROW_HEIGHT,
                      lineHeight: `${ROW_HEIGHT - 1}px`,
                    }}
                  >
                    {insert ? '+' : deleted ? '\u2212' : vr.index + 1}
                  </div>
                  {pinnedIdx.length > 0 && (
                    <div
                      className="sticky z-[9] shrink-0 border-r-2 border-border bg-surface"
                      style={{
                        left: ROWNUM_WIDTH,
                        width: pinnedWidth,
                        height: ROW_HEIGHT,
                        lineHeight: `${ROW_HEIGHT - 1}px`,
                      }}
                    >
                      {pinnedIdx.map((ci, i) =>
                        renderCell(
                          vr.index,
                          row,
                          ci,
                          pinnedWidths.slice(0, i).reduce((a, b) => a + b, 0),
                          pinnedWidths[i]!,
                          rowSelected,
                          `p${ci}`,
                          ctx,
                        ),
                      )}
                    </div>
                  )}
                  {vCols.map((vc) => {
                    const ci = freeIdx[vc.index];
                    return ci === undefined
                      ? null
                      : renderCell(vr.index, row, ci, vc.start, vc.size, rowSelected, vc.key, ctx);
                  })}
                </div>
              );
            })}
          </div>
        </div>
      </div>
      {reloading && (
        <div
          className="pointer-events-none absolute inset-x-0 flex items-center justify-center gap-2 text-muted-foreground"
          style={{ top: HEADER_HEIGHT, height: 80 }}
        >
          <Spinner /> <span>{t('grid.loading')}</span>
        </div>
      )}
    </div>
  );
}
