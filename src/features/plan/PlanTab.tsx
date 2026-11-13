import { Copy, FileDown, FolderOpen, Play, RefreshCw, Gauge } from 'lucide-react';
import { lazy, Suspense, useCallback, useState, type FC } from 'react';
import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/Badge';
import { Button } from '@/components/ui/Button';
import { ConfirmDialog } from '@/components/ui/ConfirmDialog';
import { EmptyState } from '@/components/ui/EmptyState';
import { Spinner } from '@/components/ui/Spinner';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/Tabs';
import { toast } from '@/components/ui/toastStore';
import { ipc, toIpcError } from '@/lib/ipc';
import type { PlanTabPayload } from '@/stores/tabs';
import { openPlanFile } from './open';
import { PlanDetails } from './PlanDetails';
import { PlanGraph } from './PlanGraph';
import {
  explainErrorMessage,
  flatten,
  formatCost,
  formatCount,
  formatDuration,
  isWriteStatement,
  locateRaw,
  nodeById,
} from './planModel';
import { PlanTable } from './PlanTable';
import { runPlan, usePlanStore } from './planStore';

const PlanRaw = lazy(() => import('./PlanRaw'));

type View = 'graph' | 'table' | 'raw';

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <span className="whitespace-nowrap">
      <span className="text-muted-foreground">{label} </span>
      <span className="font-medium tabular-nums">{value}</span>
    </span>
  );
}

/** A query plan: diagram, table and raw text sharing one selected operator. */
export const PlanTab: FC<PlanTabPayload> = ({ planId, sessionId, connectionId }) => {
  const { t } = useTranslation('plan');
  const entry = usePlanStore((s) => s.entries[planId]);
  const select = usePlanStore((s) => s.select);
  const [view, setView] = useState<View>('graph');
  const [collapsed, setCollapsed] = useState<ReadonlySet<number>>(() => new Set());
  const [confirm, setConfirm] = useState<'analyze' | 'rerun' | null>(null);

  const onSelect = useCallback((id: number) => select(planId, id), [select, planId]);
  const onToggle = useCallback(
    (id: number) =>
      setCollapsed((cur) => {
        const next = new Set(cur);
        if (!next.delete(id)) next.add(id);
        return next;
      }),
    [],
  );

  const tree = entry?.tree ?? null;
  const selected = tree && entry?.selectedId != null ? nodeById(tree, entry.selectedId) : undefined;
  const rawRange = entry?.raw && selected ? locateRaw(entry.raw, selected) : null;

  if (!entry) {
    return (
      <div className="flex h-full items-center justify-center" data-testid="plan-tab">
        <EmptyState title={t('status.noPlan')} />
      </div>
    );
  }

  const analyzing = entry.request?.analyze ?? false;
  const writes = isWriteStatement(entry.statement);
  const header = (
    <div className="flex shrink-0 flex-wrap items-center gap-x-4 gap-y-1 border-b border-border px-3 py-1.5 text-xs">
      <span className="min-w-0 max-w-[40ch] truncate font-mono" title={entry.statement}>
        {entry.statement.replace(/\s+/g, ' ')}
      </span>
      {entry.raw && (
        <Badge tone={entry.raw.analyzed ? 'success' : 'neutral'}>
          {entry.raw.analyzed ? t('status.analyzed') : t('status.estimated')}
        </Badge>
      )}
      {entry.raw?.rolledBack && <Badge tone="warning">{t('status.rolledBack')}</Badge>}
      {entry.raw && (
        <span className="text-muted-foreground">
          {t(`format.${entry.raw.format}`)}
          {entry.raw.serverVersion
            ? ` · ${t('status.server', { version: entry.raw.serverVersion })}`
            : ''}
        </span>
      )}
      {tree && (
        <>
          <Stat label={t('summary.totalCost')} value={formatCost(tree.totals.totalCost)} />
          {tree.totals.planningTimeMs != null && (
            <Stat
              label={t('summary.planning')}
              value={formatDuration(tree.totals.planningTimeMs)}
            />
          )}
          {tree.totals.executionTimeMs != null && (
            <Stat
              label={t('summary.execution')}
              value={formatDuration(tree.totals.executionTimeMs)}
            />
          )}
          <Stat label={t('summary.rows')} value={formatCount(tree.totals.rows)} />
          <Stat label={t('summary.nodes')} value={formatCount(tree.nodeCount)} />
        </>
      )}
      <span className="ml-auto flex items-center gap-1">
        {entry.request && (
          <>
            <Button
              size="sm"
              disabled={entry.status === 'loading'}
              onClick={() =>
                analyzing
                  ? void runPlan(planId, { analyze: false })
                  : writes
                    ? setConfirm('analyze')
                    : void runPlan(planId, { analyze: true })
              }
            >
              <Gauge size={13} />
              {analyzing ? t('toolbar.estimate') : t('toolbar.analyze')}
            </Button>
            <Button
              size="sm"
              disabled={entry.status === 'loading'}
              onClick={() => (analyzing && writes ? setConfirm('rerun') : void runPlan(planId))}
            >
              <RefreshCw size={13} />
              {t('toolbar.rerun')}
            </Button>
          </>
        )}
        <Button
          size="sm"
          disabled={!entry.raw}
          onClick={() => {
            if (!entry.raw) return;
            const raw = entry.raw;
            void ipc
              .planFileSave(raw)
              .then((path) => path && toast.success(t('toolbar.saved', { path })))
              .catch((e) => toast.error(toIpcError(e).message));
          }}
        >
          <FileDown size={13} />
          {t('toolbar.save')}
        </Button>
        <Button size="sm" onClick={() => void openPlanFile({ sessionId, connectionId })}>
          <FolderOpen size={13} />
          {t('toolbar.open')}
        </Button>
        <Button
          size="sm"
          disabled={!entry.raw}
          onClick={() => {
            if (!entry.raw) return;
            void navigator.clipboard
              ?.writeText(entry.raw.text)
              .then(() => toast.success(t('toolbar.copied')));
          }}
        >
          <Copy size={13} />
          {t('toolbar.copyRaw')}
        </Button>
      </span>
    </div>
  );

  let body;
  if (entry.status === 'loading' && !entry.raw) {
    body = (
      <div className="flex flex-1 items-center justify-center gap-2 text-sm text-muted-foreground">
        <Spinner />
        {t('status.loading')}
      </div>
    );
  } else if (entry.status === 'error') {
    body = (
      <div className="flex flex-1 items-center justify-center">
        <EmptyState
          title={t('status.failed')}
          description={entry.error ? explainErrorMessage(t, entry.error) : undefined}
          action={
            entry.request && (
              <Button onClick={() => void runPlan(planId)}>
                <Play size={13} />
                {t('toolbar.rerun')}
              </Button>
            )
          }
        />
      </div>
    );
  } else {
    const current: View = tree ? view : 'raw';
    body = (
      <Tabs
        value={current}
        onValueChange={(v) => setView(v as View)}
        className="flex min-h-0 flex-1 flex-col"
      >
        {entry.parseError && (
          <div
            className="shrink-0 border-b border-border bg-warning-soft px-3 py-1.5 text-xs text-warning"
            role="alert"
          >
            {t('status.parseError', { message: entry.parseError })}
          </div>
        )}
        <div className="flex shrink-0 items-end justify-between border-b border-border pr-2">
          <TabsList className="border-b-0" aria-label={t('views.label')}>
            <TabsTrigger value="graph" disabled={!tree}>
              {t('views.graph')}
            </TabsTrigger>
            <TabsTrigger value="table" disabled={!tree}>
              {t('views.table')}
            </TabsTrigger>
            <TabsTrigger value="raw" disabled={!entry.raw}>
              {t('views.raw')}
            </TabsTrigger>
          </TabsList>
          {current === 'table' && tree && (
            <span className="flex gap-1 pb-1">
              <Button size="sm" variant="ghost" onClick={() => setCollapsed(new Set())}>
                {t('toolbar.expandAll')}
              </Button>
              <Button
                size="sm"
                variant="ghost"
                onClick={() =>
                  setCollapsed(
                    new Set(
                      flatten(tree.root)
                        .filter((n) => n.children.length > 0)
                        .map((n) => n.id),
                    ),
                  )
                }
              >
                {t('toolbar.collapseAll')}
              </Button>
            </span>
          )}
        </div>
        <div className="flex min-h-0 flex-1">
          <div className="min-w-0 flex-1">
            {tree && (
              <TabsContent value="graph" className="h-full">
                <PlanGraph tree={tree} selectedId={entry.selectedId} onSelect={onSelect} />
              </TabsContent>
            )}
            {tree && (
              <TabsContent value="table" className="h-full">
                <PlanTable
                  tree={tree}
                  selectedId={entry.selectedId}
                  onSelect={onSelect}
                  collapsed={collapsed}
                  onToggle={onToggle}
                />
              </TabsContent>
            )}
            {entry.raw && (
              <TabsContent value="raw" className="h-full">
                <Suspense fallback={<Spinner className="m-4" />}>
                  <PlanRaw raw={entry.raw} range={rawRange} />
                </Suspense>
              </TabsContent>
            )}
          </div>
          {tree && <PlanDetails tree={tree} selected={selected} onSelect={onSelect} />}
        </div>
      </Tabs>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="plan-tab">
      {header}
      {body}
      <ConfirmDialog
        open={confirm !== null}
        onOpenChange={(open) => !open && setConfirm(null)}
        title={t('analyzeConfirm.title')}
        description={writes ? t('analyzeConfirm.write') : t('analyzeConfirm.read')}
        confirmLabel={t('analyzeConfirm.confirm')}
        destructive={writes}
        onConfirm={() => void runPlan(planId, { analyze: true })}
      />
    </div>
  );
};
