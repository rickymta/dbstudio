import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Select } from '@/components/ui/Select';
import { cn } from '@/lib/cn';
import { comparePlans, type TotalDiff } from './compareModel';
import { formatCost, formatCount, formatDuration, objectLabel } from './planModel';
import { usePlanStore } from './planStore';

const fmt: Record<TotalDiff['key'], (n: number | null) => string> = {
  totalCost: formatCost,
  planningTimeMs: formatDuration,
  executionTimeMs: formatDuration,
  rows: formatCount,
  nodeCount: formatCount,
  warnings: formatCount,
};

function label(e: { planId: string; statement: string }): string {
  const text = e.statement.replace(/\s+/g, ' ').trim();
  return text.length > 60 ? `${text.slice(0, 59)}…` : text || e.planId;
}

/** Side-by-side comparison of this plan with another open plan. */
export function PlanCompare({
  planId,
  open,
  onOpenChange,
}: {
  planId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const { t } = useTranslation('plan');
  // Narrow selectors: strings and trees only, so selecting a node in some other tab does not
  // re-render this dialog. Entries are listed as `planId\u0000statement`.
  const listed = usePlanStore((s) =>
    Object.values(s.entries)
      .filter((e) => e.planId !== planId && e.tree)
      .map((e) => `${e.planId}\u0000${e.statement}`)
      .join('\u0001'),
  );
  const others = listed
    ? listed.split('\u0001').map((x) => {
        const [id = '', statement = ''] = x.split('\u0000');
        return { planId: id, statement };
      })
    : [];
  const [otherId, setOtherId] = useState<string | undefined>(undefined);
  const other = others.find((e) => e.planId === otherId) ?? others[0];
  const currentTree = usePlanStore((s) => s.entries[planId]?.tree ?? null);
  const otherTree = usePlanStore((s) => (other ? (s.entries[other.planId]?.tree ?? null) : null));

  const result = currentTree && otherTree ? comparePlans(currentTree, otherTree) : null;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title={t('compare.title')}
        description={t('compare.description')}
        className="w-[min(860px,94vw)]"
        footer={<Button onClick={() => onOpenChange(false)}>{t('compare.close')}</Button>}
      >
        {others.length === 0 || !other || !result ? (
          <p className="text-xs text-muted-foreground">{t('compare.none')}</p>
        ) : (
          <div className="space-y-4 text-xs" data-testid="plan-compare">
            <label className="flex items-center gap-2">
              <span className="shrink-0 text-muted-foreground">{t('compare.with')}</span>
              <Select
                aria-label={t('compare.with')}
                value={other.planId}
                onValueChange={setOtherId}
                options={others.map((e) => ({ value: e.planId, label: label(e) }))}
              />
            </label>
            <table className="w-full border-collapse" data-testid="plan-compare-totals">
              <thead className="text-left text-muted-foreground">
                <tr>
                  <th className="py-1 font-medium" />
                  <th className="py-1 text-right font-medium">{t('compare.current')}</th>
                  <th className="py-1 text-right font-medium">{t('compare.other')}</th>
                  <th className="py-1 text-right font-medium">{t('compare.change')}</th>
                </tr>
              </thead>
              <tbody>
                {result.totals.map((d) => (
                  <tr key={d.key} className="border-t border-border/60">
                    <td className="py-1">{t(`compare.totals.${d.key}`)}</td>
                    <td className="py-1 text-right tabular-nums">{fmt[d.key](d.a)}</td>
                    <td className="py-1 text-right tabular-nums">{fmt[d.key](d.b)}</td>
                    <td
                      className={cn(
                        'py-1 text-right tabular-nums',
                        d.delta != null && d.delta < 0 && 'text-success',
                        d.delta != null && d.delta > 0 && 'text-danger',
                      )}
                    >
                      {d.ratio != null ? `${d.ratio.toFixed(2)}x` : '-'}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            <table className="w-full border-collapse" data-testid="plan-compare-nodes">
              <thead className="text-left text-muted-foreground">
                <tr>
                  <th className="py-1 font-medium">{t('columns.operation')}</th>
                  <th className="py-1 font-medium">{t('columns.object')}</th>
                  <th className="py-1 text-right font-medium">{t('compare.current')}</th>
                  <th className="py-1 text-right font-medium">{t('compare.other')}</th>
                  <th className="py-1 font-medium">{t('compare.status')}</th>
                </tr>
              </thead>
              <tbody>
                {result.nodes.map((d, i) => {
                  const n = d.a ?? d.b!;
                  const value = (x: typeof d.a) =>
                    x == null
                      ? '-'
                      : result.metric === 'time'
                        ? formatDuration(x.selfTimeMs)
                        : formatCost(x.selfCost);
                  return (
                    <tr
                      key={i}
                      className="border-t border-border/60"
                      data-status={d.a && d.b ? 'both' : d.a ? 'onlyCurrent' : 'onlyOther'}
                    >
                      <td className="py-1">{n.op}</td>
                      <td className="py-1 text-muted-foreground">{objectLabel(n)}</td>
                      <td className="py-1 text-right tabular-nums">{value(d.a)}</td>
                      <td className="py-1 text-right tabular-nums">{value(d.b)}</td>
                      <td className="py-1">
                        {d.a && d.b
                          ? t('compare.both')
                          : d.a
                            ? t('compare.onlyCurrent')
                            : t('compare.onlyOther')}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
