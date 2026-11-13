import { AlertTriangle, Flame, Info, OctagonAlert } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';
import type { PlanNode, PlanTree, PlanWarning, Severity } from '@/lib/types';
import {
  formatCost,
  formatCount,
  formatDuration,
  nodeShare,
  objectLabel,
  warningMessage,
  warningsByNode,
  formatPercent,
} from './planModel';

const SEVERITY_STYLE: Record<Severity, string> = {
  info: 'text-muted-foreground',
  warning: 'text-warning',
  critical: 'text-danger',
};

export function SeverityIcon({ severity, size = 14 }: { severity: Severity; size?: number }) {
  const { t } = useTranslation('plan');
  const Icon =
    severity === 'critical' ? OctagonAlert : severity === 'warning' ? AlertTriangle : Info;
  return (
    <Icon
      size={size}
      className={cn('shrink-0', SEVERITY_STYLE[severity])}
      aria-label={t(`severity.${severity}`)}
    />
  );
}

/** One warning line: icon and localized message. */
export function WarningLine({ warning, onClick }: { warning: PlanWarning; onClick?: () => void }) {
  const { t } = useTranslation('plan');
  const body = (
    <>
      <SeverityIcon severity={warning.severity} />
      <span className="min-w-0 flex-1 text-left">{warningMessage(t, warning)}</span>
    </>
  );
  return onClick ? (
    <button
      type="button"
      onClick={onClick}
      className="flex w-full items-start gap-1.5 rounded px-1.5 py-1 text-xs hover:bg-surface-muted"
    >
      {body}
    </button>
  ) : (
    <div className="flex items-start gap-1.5 px-1.5 py-1 text-xs">{body}</div>
  );
}

interface Fact {
  label: string;
  value: string;
  mono?: boolean;
}

function facts(t: (k: string) => string, tree: PlanTree, node: PlanNode): Fact[] {
  const out: Fact[] = [];
  const add = (label: string, value: string | null | undefined, mono = false) => {
    if (value && value !== '-') out.push({ label, value, mono });
  };
  add(t('node.object'), objectLabel(node));
  add(t('node.index'), node.object?.index ?? null);
  add(t('node.condition'), node.condition, true);
  add(t('node.estCost'), formatCost(node.estCost));
  add(t('node.startupCost'), formatCost(node.estStartupCost));
  add(t('node.selfCost'), formatCost(node.selfCost));
  add(t('node.estRows'), formatCount(node.estRows));
  add(t('node.actualRows'), formatCount(node.actualRows));
  add(t('node.actualRowsTotal'), formatCount(node.actualRowsTotal));
  add(t('node.loops'), node.loops == null ? null : formatCount(node.loops));
  add(t('node.time'), formatDuration(node.actualTimeMs));
  if (node.selfTimeMs != null) {
    const share = nodeShare(tree, node);
    add(
      t('node.selfTime'),
      `${formatDuration(node.selfTimeMs)}${share != null ? ` (${formatPercent(share)})` : ''}`,
    );
  }
  return out;
}

/** Key figures of a node; `compact` leaves out the raw attributes (used by the graph tooltip). */
export function NodeFacts({
  tree,
  node,
  compact = false,
}: {
  tree: PlanTree;
  node: PlanNode;
  compact?: boolean;
}) {
  const { t } = useTranslation('plan');
  const rows = facts(t as (k: string) => string, tree, node);
  const warnings = warningsByNode(tree).get(node.id) ?? [];
  const extra = Object.entries(node.extra);
  const hot = tree.hotPath.includes(node.id);
  return (
    <div className="space-y-2 text-xs" data-testid="plan-node-facts">
      <div className="flex items-center gap-1.5">
        <span className="font-semibold">{node.op}</span>
        {hot && (
          <span className="inline-flex items-center gap-0.5 text-accent">
            <Flame size={12} aria-hidden />
            {t('node.onHotPath')}
          </span>
        )}
      </div>
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1">
        {rows.map((r) => (
          <div key={r.label} className="contents">
            <dt className="text-muted-foreground">{r.label}</dt>
            <dd className={cn('min-w-0 break-words', r.mono && 'font-mono text-[11px]')}>
              {r.value}
            </dd>
          </div>
        ))}
      </dl>
      {warnings.length > 0 && (
        <div>
          {warnings.map((w, i) => (
            <WarningLine key={i} warning={w} />
          ))}
        </div>
      )}
      {!compact && extra.length > 0 && (
        <div>
          <p className="mb-1 font-medium">{t('node.attributes')}</p>
          <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5">
            {extra.map(([k, v]) => (
              <div key={k} className="contents">
                <dt className="break-all text-muted-foreground">{k}</dt>
                <dd className="min-w-0 break-words font-mono text-[11px]">{v}</dd>
              </div>
            ))}
          </dl>
        </div>
      )}
    </div>
  );
}

interface PlanDetailsProps {
  tree: PlanTree;
  selected: PlanNode | undefined;
  onSelect: (id: number) => void;
}

/** Side panel: every warning of the plan, then the figures of the selected operator. */
export function PlanDetails({ tree, selected, onSelect }: PlanDetailsProps) {
  const { t } = useTranslation('plan');
  return (
    <aside
      className="flex h-full w-72 shrink-0 flex-col overflow-y-auto border-l border-border bg-surface"
      aria-label={t('node.title')}
      data-testid="plan-details"
    >
      <section className="border-b border-border p-2">
        <h3 className="mb-1 px-1.5 text-xs font-medium">
          {tree.warnings.length > 0
            ? t('summary.warnings', { count: tree.warnings.length })
            : t('summary.noWarnings')}
        </h3>
        {tree.warnings.map((w, i) => (
          <WarningLine key={i} warning={w} onClick={() => onSelect(w.nodeId)} />
        ))}
      </section>
      <section className="p-3">
        {selected ? (
          <NodeFacts tree={tree} node={selected} />
        ) : (
          <p className="text-xs text-muted-foreground">{t('node.none')}</p>
        )}
      </section>
    </aside>
  );
}
