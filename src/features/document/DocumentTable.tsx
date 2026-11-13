import { ArrowDown, ArrowUp } from 'lucide-react';
import { observeElementRect, useVirtualizer } from '@tanstack/react-virtual';
import { useEffect, useRef } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';
import type { TimestampDisplay, Value } from '@/lib/types';
import { formatCell } from '@/features/data/format';
import type { GridModel } from '@/features/data/gridModel';
import { docFields, summarizeCell, type FlatColumn } from './flatten';
import { idKey } from './store';

/** Fixed row height in px (cells are single-line and truncated), which keeps scrolling exact. */
const ROW_HEIGHT = 24;
/** Used when the scroll element reports no size (jsdom / detached): renders a small window. */
const FALLBACK_RECT = { width: 1000, height: 600 };
/** The next page is requested when this many rows (or fewer) remain below the last drawn row. */
const LOAD_AHEAD = 20;

/** Key -> value lookup per document, built once per document object (first duplicate key wins). */
const fieldMaps = new WeakMap<object, Map<string, Value>>();

function fieldMapOf(doc: Value | undefined): Map<string, Value> | null {
  const fields = docFields(doc);
  if (!fields || !doc) return null;
  let map = fieldMaps.get(doc);
  if (!map) {
    map = new Map();
    for (const [k, v] of fields) if (!map.has(k)) map.set(k, v);
    fieldMaps.set(doc, map);
  }
  return map;
}

interface DocumentTableProps {
  model: GridModel;
  columns: FlatColumn[];
  selectedId: string | null;
  timestamp: TimestampDisplay;
  onSelect: (row: number) => void;
  onOpen: (row: number) => void;
}

function Cell({ value, timestamp }: { value: Value | undefined; timestamp: TimestampDisplay }) {
  const { t } = useTranslation('document');
  const s = summarizeCell(value);
  switch (s.kind) {
    case 'missing':
      return <span className="text-muted-foreground/50" title={t('cell.missing')} />;
    case 'null':
      return <span className="text-muted-foreground italic">{t('cell.null')}</span>;
    case 'document':
      return (
        <span className="text-muted-foreground">{`{${t(s.count === 1 ? 'cell.fieldOne' : 'cell.fieldMany', { count: s.count })}}`}</span>
      );
    case 'array':
      return (
        <span className="text-muted-foreground">{`[${t(s.count === 1 ? 'cell.itemOne' : 'cell.itemMany', { count: s.count })}]`}</span>
      );
    case 'scalar': {
      const d = formatCell(s.value, undefined, timestamp);
      return (
        <span className={cn(d.align === 'right' && 'block text-right tabular-nums')}>
          {d.badge && (
            <span className="mr-1 rounded bg-surface-muted px-1 text-[10px] text-muted-foreground">
              {d.badge}
            </span>
          )}
          {d.text}
        </span>
      );
    }
  }
}

/** Compact flat table of the loaded documents (first-level keys as columns). */
export function DocumentTable({
  model,
  columns,
  selectedId,
  timestamp,
  onSelect,
  onOpen,
}: DocumentTableProps) {
  const { t } = useTranslation('document');
  const rows = model.rows;
  const scrollRef = useRef<HTMLDivElement>(null);
  const lastRect = useRef<{ width: number; height: number } | null>(null);

  // eslint-disable-next-line react-hooks/incompatible-library
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 12,
    initialRect: FALLBACK_RECT,
    // A detached or hidden element reports 0x0: keep the last real size instead.
    observeElementRect: (inst, cb) =>
      observeElementRect(inst, (r) => {
        if (r.width === 0 && r.height === 0) return cb(lastRect.current ?? FALLBACK_RECT);
        lastRect.current = r;
        cb(r);
      }),
  });
  const items = virtualizer.getVirtualItems();
  const padTop = items.length > 0 ? items[0]!.start : 0;
  const padBottom =
    items.length > 0 ? virtualizer.getTotalSize() - items[items.length - 1]!.end : 0;
  const lastIndex = items.length > 0 ? items[items.length - 1]!.index : -1;

  // Near the bottom of the loaded rows: ask for the next page (the model ignores repeats).
  useEffect(() => {
    if (rows.length > 0 && lastIndex >= rows.length - 1 - LOAD_AHEAD) model.loadMore();
  }, [lastIndex, rows.length, model]);

  const sortOf = (name: string) => model.sort.find((s) => s.column === name);

  return (
    <div ref={scrollRef} className="min-h-0 flex-1 overflow-auto">
      <table aria-label={t('table.label')} className="border-collapse text-xs" role="grid">
        <thead className="sticky top-0 z-10 bg-surface-muted">
          <tr>
            <th className="w-12 border-b border-border px-2 py-1 text-right font-normal text-muted-foreground">
              {t('table.rowNumber')}
            </th>
            {columns.map((c) => {
              const sort = sortOf(c.name);
              return (
                <th
                  key={c.name}
                  scope="col"
                  aria-sort={sort ? (sort.descending ? 'descending' : 'ascending') : undefined}
                  className="border-b border-l border-border px-0 text-left font-medium"
                >
                  <button
                    type="button"
                    disabled={!c.sortable}
                    title={c.sortable ? undefined : t('table.notSortable')}
                    onClick={(e) => model.toggleSort(c.name, e.shiftKey)}
                    className="flex w-full items-center gap-1 px-2 py-1 text-left enabled:hover:bg-surface"
                  >
                    <span className="truncate">{c.name}</span>
                    {sort &&
                      (sort.descending ? (
                        <ArrowDown size={11} aria-label={t('table.sortDescending')} />
                      ) : (
                        <ArrowUp size={11} aria-label={t('table.sortAscending')} />
                      ))}
                  </button>
                </th>
              );
            })}
          </tr>
        </thead>
        <tbody>
          {padTop > 0 && (
            <tr aria-hidden style={{ height: padTop }}>
              <td colSpan={columns.length + 1} />
            </tr>
          )}
          {items.map((item) => {
            const i = item.index;
            const doc = rows[i]![0];
            const fields = fieldMapOf(doc);
            const key = idKey(doc);
            const selected = key !== null && key === selectedId;
            return (
              <tr
                key={i}
                aria-selected={selected}
                onClick={() => onSelect(i)}
                onDoubleClick={() => onOpen(i)}
                style={{ height: ROW_HEIGHT }}
                className={cn(
                  'cursor-default border-b border-border/50',
                  selected ? 'bg-accent/15' : 'hover:bg-surface-muted',
                )}
              >
                <td className="px-2 py-0.5 text-right text-muted-foreground tabular-nums">
                  {i + 1}
                </td>
                {columns.map((c) => (
                  <td
                    key={c.name}
                    role="gridcell"
                    className="max-w-72 truncate border-l border-border/50 px-2 py-0.5 font-mono"
                  >
                    <Cell value={fields?.get(c.name)} timestamp={timestamp} />
                  </td>
                ))}
              </tr>
            );
          })}
          {padBottom > 0 && (
            <tr aria-hidden style={{ height: padBottom }}>
              <td colSpan={columns.length + 1} />
            </tr>
          )}
        </tbody>
      </table>
    </div>
  );
}
