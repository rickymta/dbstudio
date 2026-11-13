import { ArrowDown, ArrowUp } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';
import type { TimestampDisplay, Value } from '@/lib/types';
import { formatCell } from '@/features/data/format';
import type { GridModel } from '@/features/data/gridModel';
import { docFields, fieldOf, summarizeCell, type FlatColumn } from './flatten';
import { idKey } from './store';

/** Rows drawn at first; scrolling to the bottom reveals more (and loads the next page). */
export const ROW_CHUNK = 200;

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
  const [shown, setShown] = useState(ROW_CHUNK);
  const rows = model.rows;

  const onScroll = (e: React.UIEvent<HTMLDivElement>) => {
    const el = e.currentTarget;
    if (el.scrollHeight - el.scrollTop - el.clientHeight > 200) return;
    if (shown < rows.length) setShown((n) => n + ROW_CHUNK);
    else model.loadMore();
  };

  const sortOf = (name: string) => model.sort.find((s) => s.column === name);

  return (
    <div className="min-h-0 flex-1 overflow-auto" onScroll={onScroll}>
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
          {rows.slice(0, shown).map((row, i) => {
            const doc = row[0];
            const fields = docFields(doc);
            const key = idKey(doc);
            const selected = key !== null && key === selectedId;
            return (
              <tr
                key={i}
                aria-selected={selected}
                onClick={() => onSelect(i)}
                onDoubleClick={() => onOpen(i)}
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
                    <Cell
                      value={fields ? fieldOf(fields, c.name) : undefined}
                      timestamp={timestamp}
                    />
                  </td>
                ))}
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
