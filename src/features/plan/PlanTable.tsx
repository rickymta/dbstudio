import { ChevronDown, ChevronRight, Flame } from 'lucide-react';
import { useMemo, type KeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';
import type { PlanNode, PlanTree } from '@/lib/types';
import { SeverityIcon } from './PlanDetails';
import {
  formatCost,
  formatCount,
  formatDuration,
  formatPercent,
  maxSeverity,
  metricOf,
  nodeShare,
  objectLabel,
  warningsByNode,
} from './planModel';

interface Row {
  node: PlanNode;
  depth: number;
}

function visibleRows(root: PlanNode, collapsed: ReadonlySet<number>): Row[] {
  const rows: Row[] = [];
  const walk = (n: PlanNode, depth: number) => {
    rows.push({ node: n, depth });
    if (!collapsed.has(n.id)) n.children.forEach((c) => walk(c, depth + 1));
  };
  walk(root, 0);
  return rows;
}

interface PlanTableProps {
  tree: PlanTree;
  selectedId: number | null;
  onSelect: (id: number) => void;
  collapsed: ReadonlySet<number>;
  onToggle: (id: number) => void;
}

/** The operators as an indented tree table. */
export function PlanTable({ tree, selectedId, onSelect, collapsed, onToggle }: PlanTableProps) {
  const { t } = useTranslation('plan');
  const rows = useMemo(() => visibleRows(tree.root, collapsed), [tree, collapsed]);
  const warnings = useMemo(() => warningsByNode(tree), [tree]);
  const hot = useMemo(() => new Set(tree.hotPath), [tree]);
  const metric = metricOf(tree);

  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    e.preventDefault();
    const at = rows.findIndex((r) => r.node.id === selectedId);
    const next =
      rows[Math.max(0, Math.min(rows.length - 1, at + (e.key === 'ArrowDown' ? 1 : -1)))];
    if (next) onSelect(next.node.id);
  };

  return (
    <div className="h-full overflow-auto" onKeyDown={onKeyDown}>
      <table className="w-full min-w-[760px] border-collapse text-xs" data-testid="plan-table">
        <thead className="sticky top-0 z-10 bg-surface-muted text-left text-muted-foreground">
          <tr>
            <th className="px-2 py-1 font-medium">{t('columns.operation')}</th>
            <th className="px-2 py-1 font-medium">{t('columns.object')}</th>
            <th className="px-2 py-1 text-right font-medium">{t('columns.estRows')}</th>
            <th className="px-2 py-1 text-right font-medium">{t('columns.actualRows')}</th>
            <th className="px-2 py-1 text-right font-medium">{t('columns.loops')}</th>
            <th className="px-2 py-1 text-right font-medium">
              {metric === 'time' ? t('columns.selfTime') : t('columns.cost')}
            </th>
            <th className="w-28 px-2 py-1 font-medium">{t('columns.share')}</th>
          </tr>
        </thead>
        <tbody>
          {rows.map(({ node, depth }) => {
            const share = nodeShare(tree, node);
            const severity = maxSeverity(warnings.get(node.id));
            const selected = node.id === selectedId;
            const isHot = hot.has(node.id);
            return (
              <tr
                key={node.id}
                data-testid={`plan-row-${node.id}`}
                aria-selected={selected}
                data-hot={isHot || undefined}
                tabIndex={selected ? 0 : -1}
                onClick={() => onSelect(node.id)}
                className={cn(
                  'cursor-pointer border-b border-border/60 hover:bg-surface-muted',
                  selected && 'bg-warning-soft hover:bg-warning-soft',
                  isHot && 'font-semibold',
                )}
              >
                <td className="px-2 py-1">
                  <span className="flex items-center gap-1" style={{ paddingLeft: depth * 14 }}>
                    {node.children.length > 0 ? (
                      <button
                        type="button"
                        aria-label={collapsed.has(node.id) ? t('node.expand') : t('node.collapse')}
                        aria-expanded={!collapsed.has(node.id)}
                        className="rounded p-0.5 hover:bg-border"
                        onClick={(e) => {
                          e.stopPropagation();
                          onToggle(node.id);
                        }}
                      >
                        {collapsed.has(node.id) ? (
                          <ChevronRight size={12} />
                        ) : (
                          <ChevronDown size={12} />
                        )}
                      </button>
                    ) : (
                      <span className="inline-block w-4" />
                    )}
                    {isHot && (
                      <Flame size={12} className="text-accent" aria-label={t('status.hotPath')} />
                    )}
                    <span className="truncate">{node.op}</span>
                    {severity && <SeverityIcon severity={severity} size={12} />}
                  </span>
                </td>
                <td className="max-w-56 truncate px-2 py-1 text-muted-foreground">
                  {objectLabel(node)}
                </td>
                <td className="px-2 py-1 text-right tabular-nums">{formatCount(node.estRows)}</td>
                <td className="px-2 py-1 text-right tabular-nums">
                  {formatCount(node.actualRows)}
                </td>
                <td className="px-2 py-1 text-right tabular-nums">{formatCount(node.loops)}</td>
                <td className="px-2 py-1 text-right tabular-nums">
                  {metric === 'time' ? formatDuration(node.selfTimeMs) : formatCost(node.selfCost)}
                </td>
                <td className="px-2 py-1">
                  {share != null && (
                    <span className="flex items-center gap-1.5">
                      <span className="h-1.5 flex-1 overflow-hidden rounded bg-surface-muted">
                        <span
                          className="block h-full bg-accent"
                          style={{ width: `${Math.max(2, share * 100)}%` }}
                        />
                      </span>
                      <span className="w-9 text-right tabular-nums text-muted-foreground">
                        {formatPercent(share)}
                      </span>
                    </span>
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
