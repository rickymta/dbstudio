import {
  ChevronDown,
  ChevronRight,
  ClipboardCopy,
  Lightbulb,
  SquarePen,
  Table2,
  X,
} from 'lucide-react';
import { useId, useState, type KeyboardEvent, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/Badge';
import { Button } from '@/components/ui/Button';
import { Spinner } from '@/components/ui/Spinner';
import { cn } from '@/lib/cn';
import type { AdviceAction, PlanAdvice, PlanTree } from '@/lib/types';
import {
  actionText,
  adviceShareText,
  adviceTitle,
  adviceWhy,
  confidenceLabel,
  failedTablesText,
  formatEvidenceValue,
  isDdlAction,
  primaryAction,
  splitStatement,
  summaryParts,
  type AdviceStatus,
} from './adviceModel';
import { SeverityIcon, WarningLine } from './PlanDetails';
import { useAdviceActions, type AdviceActions } from './PlanAdviceActions';

/** `reviewFilter` -> `Review filter` when a label key has no translation yet. */
function humanize(key: string): string {
  const spaced = key.replace(/([a-z0-9])([A-Z])/g, '$1 $2').toLowerCase();
  return spaced.charAt(0).toUpperCase() + spaced.slice(1);
}

const CONFIDENCE_STYLE = {
  high: 'bg-success-soft text-success',
  medium: 'bg-warning-soft text-warning',
  verify: 'border border-dashed border-border bg-transparent text-muted-foreground',
} as const;

function ConfidenceBadge({ advice }: { advice: PlanAdvice }) {
  const { t } = useTranslation('plan');
  const label = confidenceLabel(advice);
  return (
    <span
      data-testid="advice-confidence"
      data-confidence={label}
      className={cn(
        'inline-flex shrink-0 items-center whitespace-nowrap rounded px-1.5 py-0.5 text-[11px] font-medium leading-none',
        CONFIDENCE_STYLE[label],
      )}
    >
      {t(`advice.confidence.${label}`)}
    </span>
  );
}

function ActionBlock({ action, actions }: { action: AdviceAction; actions: AdviceActions }) {
  const { t } = useTranslation('plan');
  const text = actionText(action);
  const ddl = isDdlAction(action);
  return (
    <div className="space-y-1" data-testid="advice-action" data-kind={action.kind}>
      <div className="flex flex-wrap items-center gap-1">
        <span className="font-medium">
          {t(`advice.actions.${action.labelKey}`, { defaultValue: humanize(action.labelKey) })}
        </span>
        <Badge>{t(`advice.scope.${action.scope}`)}</Badge>
        {action.lock && (
          <Badge tone={action.lock === 'none' ? 'neutral' : 'warning'}>
            {t(`advice.lock.${action.lock}`)}
          </Badge>
        )}
        {actions.production && ddl && (
          <span
            data-testid="advice-production"
            className="inline-flex items-center rounded bg-danger-soft px-1.5 py-0.5 text-[11px] font-medium leading-none text-danger"
          >
            {t('advice.productionBadge')}
          </span>
        )}
        {action.template && (
          <Badge tone="warning" data-testid="advice-template">
            {t('advice.template')}
          </Badge>
        )}
      </div>
      {actions.production && ddl && (
        <p className="text-[11px] text-danger">{t('advice.productionNote')}</p>
      )}
      {text && (
        <>
          <pre
            className="max-h-40 overflow-auto whitespace-pre-wrap break-words rounded border border-border bg-surface-muted p-1.5 font-mono text-[11px]"
            data-testid="advice-sql"
          >
            {text}
          </pre>
          <div className="flex flex-wrap gap-1">
            <Button size="sm" onClick={() => actions.copy(action)}>
              <ClipboardCopy size={12} aria-hidden />
              {t('advice.actions.copy')}
            </Button>
            <Button size="sm" onClick={() => actions.insert(action)}>
              <SquarePen size={12} aria-hidden />
              {t('advice.actions.insert')}
            </Button>
          </div>
        </>
      )}
    </div>
  );
}

interface CardProps {
  advice: PlanAdvice;
  statement: string;
  expanded: boolean;
  actions: AdviceActions;
  onActivate: (advice: PlanAdvice) => void;
}

/** One piece of advice: a header that stays visible and a body with evidence, actions, caveats. */
export function AdviceCard({ advice, statement, expanded, actions, onActivate }: CardProps) {
  const { t } = useTranslation('plan');
  const bodyId = useId();
  const share = adviceShareText(advice);
  const hasDdl = advice.actions.some((a) => isDdlAction(a) && actionText(a));
  const range = advice.statementRange;
  const snippet = range ? splitStatement(statement, range) : null;
  const multi = advice.nodeIds.length > 1;
  return (
    <li
      data-testid={`advice-card-${advice.id}`}
      data-severity={advice.severity}
      className="rounded-md border border-border bg-surface"
    >
      <button
        type="button"
        data-advice-header
        aria-expanded={expanded}
        aria-controls={bodyId}
        onClick={() => onActivate(advice)}
        className="flex w-full items-start gap-1.5 rounded-md px-2 py-1.5 text-left text-xs hover:bg-surface-muted"
      >
        <span className="mt-0.5">
          <SeverityIcon severity={advice.severity} />
        </span>
        <span className="min-w-0 flex-1">
          <span className="block font-medium">{adviceTitle(t, advice)}</span>
          <span className="mt-1 flex flex-wrap items-center gap-1">
            <ConfidenceBadge advice={advice} />
            {share && (
              <span
                className="text-[11px] tabular-nums text-muted-foreground"
                data-testid="advice-share"
              >
                ~{share}
              </span>
            )}
          </span>
        </span>
        {expanded ? (
          <ChevronDown size={14} className="mt-0.5 shrink-0" aria-hidden />
        ) : (
          <ChevronRight size={14} className="mt-0.5 shrink-0" aria-hidden />
        )}
      </button>
      {expanded && (
        <div id={bodyId} className="space-y-2.5 border-t border-border px-2 py-2 text-xs">
          <section>
            <h4 className="mb-0.5 font-semibold">{t('advice.why')}</h4>
            <p>{adviceWhy(t, advice)}</p>
            {advice.evidence.length > 0 && (
              <dl
                className="mt-1.5 grid grid-cols-[1fr_auto] gap-x-3 gap-y-0.5"
                data-testid="advice-evidence"
              >
                {advice.evidence.map((e, i) => (
                  <div key={`${e.key}-${i}`} className="contents">
                    <dt className="text-muted-foreground">
                      {t(`advice.evidence.${e.key}`, { defaultValue: humanize(e.key) })}
                      {multi && e.nodeId != null && ` (#${e.nodeId})`}
                    </dt>
                    <dd className="text-right tabular-nums">
                      {formatEvidenceValue(e)}
                      {!e.actual && (
                        <span className="text-muted-foreground"> {t('advice.estimateSuffix')}</span>
                      )}
                    </dd>
                  </div>
                ))}
              </dl>
            )}
          </section>
          {snippet && (
            <pre
              className="max-h-32 overflow-auto whitespace-pre-wrap break-words rounded border border-border bg-surface-muted p-1.5 font-mono text-[11px]"
              data-testid="advice-statement"
            >
              {snippet.before}
              <mark className="bg-warning-soft text-foreground">{snippet.hit}</mark>
              {snippet.after}
            </pre>
          )}
          {advice.actions.length > 0 && (
            <section className="space-y-2">
              <h4 className="font-semibold">{t('advice.what')}</h4>
              {actions.readOnly && hasDdl && (
                <p
                  role="alert"
                  data-testid="advice-readonly"
                  className="rounded bg-warning-soft px-2 py-1 text-warning"
                >
                  {t('advice.readOnlyBanner')}
                </p>
              )}
              {advice.actions.map((a, i) => (
                <ActionBlock key={`${a.kind}-${i}`} action={a} actions={actions} />
              ))}
            </section>
          )}
          {advice.caveats.length > 0 && (
            <section>
              <h4 className="mb-0.5 font-semibold">{t('advice.caveatsTitle')}</h4>
              <ul className="list-disc space-y-0.5 pl-4" data-testid="advice-caveats">
                {advice.caveats.map((c, i) => (
                  <li key={`${c.key}-${i}`} data-verify={c.verify || undefined}>
                    {t(`advice.caveats.${c.key}`, {
                      ...(c.params ?? {}),
                      defaultValue: humanize(c.key),
                    })}
                    {c.verify && (
                      <span className="text-muted-foreground">
                        {' '}
                        · {t('advice.confidence.verify')}
                      </span>
                    )}
                  </li>
                ))}
              </ul>
            </section>
          )}
          {advice.table?.path && (
            <Button
              size="sm"
              disabled={!actions.canOpenTable}
              onClick={() => actions.openTable(advice)}
            >
              <Table2 size={12} aria-hidden />
              {t('advice.actions.openTable')}
            </Button>
          )}
        </div>
      )}
    </li>
  );
}

/** One line telling how far the catalog matching (tier 2) got. */
export function AdviceStatusLine({ status }: { status: AdviceStatus }) {
  const { t } = useTranslation('plan');
  if (status.kind === 'idle') return null;
  const tooltip = failedTablesText(status);
  const count = status.metadata?.checked.length ?? 0;
  const text =
    status.kind === 'checking'
      ? t('advice.status.checking')
      : status.kind === 'checked'
        ? t('advice.status.checked', { count })
        : t(`advice.status.${status.kind}`, { count });
  return (
    <span
      className="inline-flex min-w-0 items-center gap-1 text-[11px] text-muted-foreground"
      data-testid="advice-status"
      data-status={status.kind}
      title={tooltip || undefined}
    >
      {status.kind === 'checking' && <Spinner size={10} />}
      <span className="truncate">{text}</span>
    </span>
  );
}

interface SummaryProps {
  ranked: readonly PlanAdvice[];
  tree: PlanTree | null;
  status: AdviceStatus;
  onView: () => void;
}

/** Strip under the header: how many points, the biggest one, and the state of tier 2. */
export function AdviceSummary({ ranked, tree, status, onView }: SummaryProps) {
  const { t } = useTranslation('plan');
  const [hidden, setHidden] = useState(false);
  const parts = summaryParts(ranked, tree);
  const top = ranked[0];
  if (!parts && hidden && status.kind === 'idle') return null;
  let body: ReactNode;
  if (parts && top) {
    const shareText = parts.share ? t(`advice.share.${parts.basis}`, { pct: parts.share }) : '';
    body = (
      <>
        <span className="min-w-0 truncate" data-testid="advice-summary-text">
          {t('advice.summary', { count: parts.count, title: adviceTitle(t, top), shareText })}
        </span>
        <Button size="sm" variant="ghost" onClick={onView} className="shrink-0">
          {t('advice.view')}
        </Button>
      </>
    );
  } else if (!hidden) {
    body = (
      <>
        <span className="min-w-0 truncate text-muted-foreground" data-testid="advice-summary-none">
          {t('advice.summaryNone')}
        </span>
        <button
          type="button"
          aria-label={t('advice.dismiss')}
          title={t('advice.dismiss')}
          onClick={() => setHidden(true)}
          className="shrink-0 rounded p-0.5 text-muted-foreground hover:bg-surface-muted"
        >
          <X size={12} aria-hidden />
        </button>
      </>
    );
  } else {
    body = null;
  }
  return (
    <div
      className="flex h-8 shrink-0 items-center gap-2 border-b border-border bg-surface-muted/50 px-3 text-xs"
      data-testid="advice-summary"
    >
      <Lightbulb size={13} className="shrink-0 text-accent" aria-hidden />
      {body}
      <span className="ml-auto flex shrink-0 items-center">
        <AdviceStatusLine status={status} />
      </span>
    </div>
  );
}

interface PanelProps {
  planId: string;
  tree: PlanTree;
  statement: string;
  /** All advice, ranked. */
  ranked: readonly PlanAdvice[];
  status: AdviceStatus;
  /** Only advice of this node is listed (the user clicked it). */
  nodeFilter: number | null;
  onClearFilter: () => void;
  expandedId: string | null;
  onActivate: (advice: PlanAdvice) => void;
  onSelectNode: (id: number) => void;
}

/** The "Advice" tab of the inspector. */
export function AdvicePanel({
  planId,
  tree,
  statement,
  ranked,
  status,
  nodeFilter,
  onClearFilter,
  expandedId,
  onActivate,
  onSelectNode,
}: PanelProps) {
  const { t } = useTranslation('plan');
  const actions = useAdviceActions(planId);
  const shown = nodeFilter == null ? ranked : ranked.filter((a) => a.nodeIds.includes(nodeFilter));

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const target = e.target as HTMLElement;
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      const headers = Array.from(
        e.currentTarget.querySelectorAll<HTMLElement>('[data-advice-header]'),
      );
      const from = headers.findIndex((h) => h.closest('li')?.contains(target));
      if (from < 0) return;
      const step = e.key === 'ArrowDown' ? 1 : -1;
      const next = headers[Math.max(0, Math.min(headers.length - 1, from + step))];
      if (next) {
        e.preventDefault();
        next.focus();
      }
    } else if (e.key === 'c' && !e.ctrlKey && !e.metaKey && !e.altKey) {
      const card = target.closest('li[data-testid^="advice-card-"]');
      const id = card?.getAttribute('data-testid')?.slice('advice-card-'.length);
      const advice = ranked.find((a) => a.id === id);
      const action = advice ? primaryAction(advice) : null;
      if (action) {
        e.preventDefault();
        actions.copy(action);
      }
    }
  };

  return (
    <div
      className="flex h-full min-h-0 flex-col overflow-y-auto"
      data-testid="advice-panel"
      onKeyDown={onKeyDown}
    >
      <div className="space-y-2 p-2">
        {nodeFilter != null && (
          <div
            className="flex items-center gap-1 rounded bg-surface-muted px-2 py-1 text-xs"
            data-testid="advice-filter"
          >
            <span className="min-w-0 flex-1 truncate">
              {t('advice.filteredBy', { node: nodeFilter })}
            </span>
            <Button size="sm" variant="ghost" onClick={onClearFilter}>
              {t('advice.showAll')}
            </Button>
          </div>
        )}
        <AdviceStatusLine status={status} />
        {shown.length === 0 ? (
          <p className="px-1 py-2 text-xs text-muted-foreground" data-testid="advice-empty">
            {nodeFilter != null ? t('advice.noneForNode') : t('advice.summaryNone')}
          </p>
        ) : (
          <ul className="space-y-1.5" aria-label={t('advice.title')}>
            {shown.map((a) => (
              <AdviceCard
                key={a.id}
                advice={a}
                statement={statement}
                expanded={expandedId === a.id}
                actions={actions}
                onActivate={onActivate}
              />
            ))}
          </ul>
        )}
      </div>
      {tree.warnings.length > 0 && (
        <details className="border-t border-border p-2" data-testid="advice-raw-warnings">
          <summary className="cursor-pointer px-1.5 text-xs font-medium">
            {t('advice.rawWarnings', { count: tree.warnings.length })}
          </summary>
          <div className="mt-1">
            {tree.warnings.map((w, i) => (
              <WarningLine key={i} warning={w} onClick={() => onSelectNode(w.nodeId)} />
            ))}
          </div>
        </details>
      )}
    </div>
  );
}
