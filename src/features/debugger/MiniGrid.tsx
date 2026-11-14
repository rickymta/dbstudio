import { ChevronDown, ChevronUp, Copy } from 'lucide-react';
import { useState, type FC } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from '@/components/ui/toastStore';
import { valueToText } from '@/features/data/format';
import type { ColumnMeta, Value } from '@/lib/types';
import { resultToTsv } from './valueText';

export const MINI_GRID_MAX_ROWS = 200;
export const MINI_GRID_COLLAPSED_ROWS = 5;

export interface MiniGridProps {
  columns: ColumnMeta[];
  rows: Value[][];
  truncated?: boolean;
  /** Rows shown while collapsed. */
  collapsedRows?: number;
  /** Start expanded (Evaluate dialog). */
  defaultExpanded?: boolean;
}

/** Compact result grid of the Console: collapsed to a few rows, expandable, copies as TSV. */
export const MiniGrid: FC<MiniGridProps> = ({
  columns,
  rows,
  truncated,
  collapsedRows = MINI_GRID_COLLAPSED_ROWS,
  defaultExpanded = false,
}) => {
  const { t } = useTranslation('debugger');
  const [expanded, setExpanded] = useState(defaultExpanded);
  const capped = rows.length > MINI_GRID_MAX_ROWS;
  const limit = expanded ? MINI_GRID_MAX_ROWS : collapsedRows;
  const shown = rows.slice(0, limit);
  const hidden = Math.min(rows.length, MINI_GRID_MAX_ROWS) - shown.length;

  const copy = () => {
    void navigator.clipboard
      ?.writeText(resultToTsv({ columns, rows, truncated: !!truncated }))
      .then(() => toast.success(t('grid.copied')))
      .catch(() => toast.error(t('grid.copyFailed')));
  };

  return (
    <div
      className="my-1 max-w-full rounded border border-border bg-surface"
      data-testid="mini-grid"
    >
      <div className="max-h-72 overflow-auto">
        <table className="w-full border-collapse text-xs" aria-label={t('grid.label')}>
          <thead className="sticky top-0 bg-surface-muted">
            <tr>
              {columns.map((c, i) => (
                <th
                  key={i}
                  scope="col"
                  className="border-b border-border px-2 py-1 text-left font-medium whitespace-nowrap"
                  title={c.typeName}
                >
                  {c.name}
                  <span className="ml-1 font-normal text-muted-foreground">{c.typeName}</span>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {shown.map((row, r) => (
              <tr key={r} className="odd:bg-surface-muted/40">
                {row.map((value, c) => (
                  <td
                    key={c}
                    className="max-w-64 truncate px-2 py-0.5 font-mono whitespace-nowrap"
                    title={valueToText(value)}
                  >
                    {value.t === 'null' ? (
                      <span className="text-muted-foreground italic">NULL</span>
                    ) : (
                      valueToText(value)
                    )}
                  </td>
                ))}
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td
                  colSpan={Math.max(columns.length, 1)}
                  className="px-2 py-1 text-muted-foreground italic"
                >
                  {t('grid.empty')}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      <div className="flex items-center gap-2 border-t border-border px-2 py-0.5 text-xs text-muted-foreground">
        <span>{t('grid.rows', { n: rows.length })}</span>
        {(truncated || capped) && <span>{t('grid.truncated')}</span>}
        <span className="flex-1" />
        {(hidden > 0 || expanded) && rows.length > collapsedRows && (
          <button
            type="button"
            className="inline-flex items-center gap-1 rounded px-1 hover:bg-surface-muted hover:text-foreground"
            aria-expanded={expanded}
            onClick={() => setExpanded((v) => !v)}
          >
            {expanded ? <ChevronUp size={12} aria-hidden /> : <ChevronDown size={12} aria-hidden />}
            {expanded ? t('grid.collapse') : t('grid.expand', { n: hidden })}
          </button>
        )}
        <button
          type="button"
          className="inline-flex items-center gap-1 rounded px-1 hover:bg-surface-muted hover:text-foreground"
          onClick={copy}
        >
          <Copy size={12} aria-hidden />
          {t('grid.copy')}
        </button>
      </div>
    </div>
  );
};
