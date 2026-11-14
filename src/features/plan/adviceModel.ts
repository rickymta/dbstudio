// Pure helpers of the Query Advisor UI: ranking, labels, evidence formatting, safety checks.

import type { TFunction } from 'i18next';
import type {
  AdviceAction,
  AdviceMetadata,
  Confidence,
  Evidence,
  PlanAdvice,
  PlanTree,
  Severity,
} from '@/lib/types';
import { formatCount, formatDuration, formatPercent, metricOf } from './planModel';

const SEVERITY_RANK: Record<Severity, number> = { info: 0, warning: 1, critical: 2 };

/** Highest-impact advice first: `impact.score` descending, ties by the first node, then input order. */
export function rankAdvice(list: readonly PlanAdvice[] | undefined): PlanAdvice[] {
  return (list ?? [])
    .map((a, i) => ({ a, i }))
    .sort(
      (x, y) =>
        y.a.impact.score - x.a.impact.score ||
        (x.a.nodeIds[0] ?? Number.MAX_SAFE_INTEGER) - (y.a.nodeIds[0] ?? Number.MAX_SAFE_INTEGER) ||
        x.i - y.i,
    )
    .map(({ a }) => a);
}

/** What the confidence badge shows: `low` or any "verify" caveat means "needs verification". */
export type ConfidenceLabel = 'high' | 'medium' | 'verify';

export function needsVerification(a: PlanAdvice): boolean {
  return a.confidence === 'low' || a.caveats.some((c) => c.verify);
}

export function confidenceLabel(a: PlanAdvice): ConfidenceLabel {
  if (needsVerification(a)) return 'verify';
  return a.confidence === 'high' ? 'high' : 'medium';
}

export const CONFIDENCE_ORDER: Record<Confidence, number> = { low: 0, medium: 1, high: 2 };

/** Nodes that carry at least one advice (for the lightbulb marks). */
export function adviceNodeSet(advice: readonly PlanAdvice[]): Set<number> {
  const out = new Set<number>();
  for (const a of advice) for (const id of a.nodeIds) out.add(id);
  return out;
}

/** Advice that concerns `nodeId` (ranked order is kept). */
export function adviceForNode(advice: readonly PlanAdvice[], nodeId: number): PlanAdvice[] {
  return advice.filter((a) => a.nodeIds.includes(nodeId));
}

/** The first advice of a ranked list, or `null`. */
export function biggestAdvice(ranked: readonly PlanAdvice[]): PlanAdvice | null {
  return ranked[0] ?? null;
}

export function maxAdviceSeverity(advice: readonly PlanAdvice[]): Severity | null {
  let best: Severity | null = null;
  for (const a of advice) {
    if (best === null || SEVERITY_RANK[a.severity] > SEVERITY_RANK[best]) best = a.severity;
  }
  return best;
}

/** Placeholders the rule texts may use; a missing param renders as an ellipsis, never `{{x}}`. */
const KNOWN_PARAMS = [
  'table',
  'column',
  'columns',
  'index',
  'usedIndex',
  'examined',
  'kept',
  'selectivity',
  'rows',
  'loops',
  'lossy',
  'recheck',
  'workMem',
  'spaceKb',
  'fetches',
  'offset',
  'function',
  'expression',
  'pattern',
  'factor',
  'estimated',
  'actual',
  'direction',
  'planned',
  'launched',
  'jit',
  'batches',
  'sign',
  'extension',
  'type',
  'collection',
  'key',
  'filter',
] as const;

/** Interpolation values of an advice text: the advice params over `…` defaults and the table. */
export function adviceParams(a: PlanAdvice): Record<string, string> {
  const out: Record<string, string> = {};
  for (const k of KNOWN_PARAMS) out[k] = '…';
  if (a.table?.name) {
    out.table = a.table.name;
    out.collection = a.table.name;
  }
  return { ...out, ...a.params };
}

export function adviceTitle(t: TFunction<'plan'>, a: PlanAdvice): string {
  return t(`plan:advice.rules.${a.rule}.title`, {
    ...adviceParams(a),
    defaultValue: t('plan:advice.rules.unknown.title'),
  });
}

export function adviceWhy(t: TFunction<'plan'>, a: PlanAdvice): string {
  return t(`plan:advice.rules.${a.rule}.why`, {
    ...adviceParams(a),
    defaultValue: t('plan:advice.rules.unknown.why'),
  });
}

const BYTE_UNITS = ['B', 'KB', 'MB', 'GB', 'TB'] as const;

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes)) return '-';
  let v = Math.abs(bytes);
  let i = 0;
  while (v >= 1024 && i < BYTE_UNITS.length - 1) {
    v /= 1024;
    i++;
  }
  const text = v >= 100 || i === 0 ? v.toFixed(0) : v.toFixed(1);
  return `${bytes < 0 ? '-' : ''}${text} ${BYTE_UNITS[i]}`;
}

/** Number of an evidence row in its unit (`text` evidence is shown as is). */
export function formatEvidenceValue(e: Evidence): string {
  if (e.value == null) return e.text ?? '-';
  const v = e.value;
  switch (e.unit) {
    case 'ms':
      return formatDuration(v);
    case 'bytes':
      return formatBytes(v);
    case 'kb':
      return formatBytes(v * 1024);
    case 'pct':
      return `${v.toFixed(v < 10 ? 1 : 0)}%`;
    case 'ratio':
      return `${v >= 100 ? v.toFixed(0) : v.toFixed(1)}x`;
    case 'rows':
    case 'blocks':
    case 'count':
    default:
      return formatCount(v);
  }
}

/** The share an advice claims, as `78%`, or `null` when unknown. */
export function adviceShareText(a: PlanAdvice): string | null {
  const s = a.impact.share;
  return s == null ? null : formatPercent(s);
}

/** Whether the statement text is DDL/maintenance that the production/read-only warning applies to. */
export function isDdlAction(action: AdviceAction): boolean {
  return (
    action.kind === 'createIndex' || action.kind === 'maintenance' || action.kind === 'statistics'
  );
}

/** Text to copy or insert; `null` for description-only actions. */
export function actionText(action: AdviceAction): string | null {
  const sql = action.sql?.trim();
  return sql ? sql : null;
}

/** First action with a statement (what the `c` shortcut copies). */
export function primaryAction(a: PlanAdvice): AdviceAction | null {
  return a.actions.find((x) => actionText(x) !== null) ?? null;
}

/** The statement shown in the summary strip: "N points · biggest: title — node takes ~78%". */
export function summaryParts(
  ranked: readonly PlanAdvice[],
  tree: PlanTree | null,
): { count: number; share: string | null; basis: 'time' | 'cost' } | null {
  const top = biggestAdvice(ranked);
  if (!top) return null;
  const basis = top.impact.basis === 'cost' ? 'cost' : top.impact.basis === 'time' ? 'time' : null;
  return {
    count: ranked.length,
    share: adviceShareText(top),
    basis: basis ?? (tree && metricOf(tree) === 'cost' ? 'cost' : 'time'),
  };
}

/** State of the second tier as the UI shows it. */
export type AdviceStatusKind =
  'idle' | 'checking' | 'checked' | 'partial' | 'unavailable' | 'disabled' | 'failed';

export interface AdviceStatus {
  kind: AdviceStatusKind;
  metadata: AdviceMetadata | null;
  /** Message of the failed call. */
  error: string | null;
}

export const IDLE_STATUS: AdviceStatus = { kind: 'idle', metadata: null, error: null };

export function statusOfReport(metadata: AdviceMetadata): AdviceStatus {
  switch (metadata.state) {
    case 'checked':
    case 'partial':
    case 'unavailable':
    case 'disabled':
      return { kind: metadata.state, metadata, error: null };
    default:
      return { kind: 'idle', metadata, error: null };
  }
}

/** Plain text of the tooltip listing the tables that could not be matched. */
export function failedTablesText(status: AdviceStatus): string {
  const failed = status.metadata?.failed ?? [];
  const parts = failed.map((f) => `${f.table}: ${f.reason}`);
  if (status.error) parts.push(status.error);
  return parts.join('\n');
}

/** Character slice of `statement` split around `[start, end)` (Unicode scalar offsets). */
export function splitStatement(
  statement: string,
  range: { start: number; end: number },
): { before: string; hit: string; after: string } {
  const chars = Array.from(statement);
  const start = Math.max(0, Math.min(range.start, chars.length));
  const end = Math.max(start, Math.min(range.end, chars.length));
  return {
    before: chars.slice(0, start).join(''),
    hit: chars.slice(start, end).join(''),
    after: chars.slice(end).join(''),
  };
}
