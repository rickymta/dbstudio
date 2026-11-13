import {
  ArrowDownUp,
  Box,
  CircleDot,
  Combine,
  Download,
  Filter,
  GitMerge,
  KeyRound,
  Layers,
  ListChecks,
  ListEnd,
  ListTree,
  Network,
  Pencil,
  Repeat,
  Search,
  Sigma,
  Table2,
  AppWindow,
  CornerDownRight,
  Database,
  type LucideIcon,
} from 'lucide-react';
import type { PlanNodeKind } from '@/lib/types';

/** Icon per operator kind (lucide). */
const KIND_ICONS: Record<PlanNodeKind, LucideIcon> = {
  fullScan: Table2,
  indexScan: ListTree,
  indexOnlyScan: ListChecks,
  indexSeek: Search,
  keyLookup: KeyRound,
  nestedLoopJoin: Repeat,
  hashJoin: Combine,
  mergeJoin: GitMerge,
  sort: ArrowDownUp,
  aggregate: Sigma,
  hashAggregate: Sigma,
  window: AppWindow,
  limit: ListEnd,
  filter: Filter,
  materialize: Database,
  subquery: CornerDownRight,
  union: Layers,
  gather: Network,
  modify: Pencil,
  fetch: Download,
  result: CircleDot,
  other: Box,
};

export function PlanNodeIcon({ kind, size = 13 }: { kind: PlanNodeKind; size?: number }) {
  const Icon = KIND_ICONS[kind];
  return (
    <Icon size={size} className="shrink-0 text-muted-foreground" aria-hidden data-kind={kind} />
  );
}
