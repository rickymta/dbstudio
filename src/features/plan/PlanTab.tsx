import {
  Copy,
  Ellipsis,
  GitCompare,
  X,
  FileDown,
  FolderOpen,
  Play,
  PlugZap,
  RefreshCw,
  Gauge,
  type LucideIcon,
} from 'lucide-react';
import { lazy, Suspense, useCallback, useRef, useState, type FC } from 'react';
import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/Badge';
import { Button } from '@/components/ui/Button';
import { ConfirmDialog } from '@/components/ui/ConfirmDialog';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/DropdownMenu';
import { EmptyState } from '@/components/ui/EmptyState';
import { IconButton } from '@/components/ui/IconButton';
import { Spinner } from '@/components/ui/Spinner';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/Tabs';
import { toast } from '@/components/ui/toastStore';
import { ipc, toIpcError } from '@/lib/ipc';
import { isSessionLost, reconnectSession } from '@/stores/sessionRecovery';
import type { PlanTabPayload } from '@/stores/tabs';
import { openPlanFile } from './open';
import { PlanCompare } from './PlanCompare';
import { PlanDetails } from './PlanDetails';
import { PlanGraph } from './PlanGraph';
import { PlanInspector } from './PlanInspector';
import { useInspectorPrefs } from './planPrefs';
import { useElementWidth } from './useElementWidth';
import {
  explainErrorMessage,
  flatten,
  formatCost,
  formatCount,
  formatDuration,
  statementWrites,
  locateRaw,
  nodeById,
} from './planModel';
import { PlanTable } from './PlanTable';
import { cancelPlan, runPlan, usePlanStore } from './planStore';

/** Plan area narrower than this: the inspector starts collapsed. */
const INSPECTOR_AUTO_COLLAPSE_PX = 1100;
/** Secondary stats (planning time, operator count) are dropped below this width. */
const STATS_MIN_PX = 900;
/** Below: secondary actions live in the ⋯ menu; up to ICON_ONLY_MAX_PX: icon-only buttons. */
const MENU_MAX_PX = 1100;
const ICON_ONLY_MAX_PX = 1280;

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
  const [comparing, setComparing] = useState(false);
  const [confirm, setConfirm] = useState<'analyze' | 'rerun' | null>(null);
  const [writes, setWrites] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  // `null` (not measured yet) counts as roomy.
  const width = useElementWidth(rootRef) ?? Number.POSITIVE_INFINITY;
  const inspector = useInspectorPrefs();
  const inspectorCollapsed =
    inspector.choice === 'open'
      ? false
      : inspector.choice === 'collapsed'
        ? true
        : width < INSPECTOR_AUTO_COLLAPSE_PX;
  const [reconnecting, setReconnecting] = useState(false);

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
      <div ref={rootRef} className="flex h-full items-center justify-center" data-testid="plan-tab">
        <EmptyState title={t('status.noPlan')} />
      </div>
    );
  }

  const analyzing = entry.request?.analyze ?? false;
  // Ask the backend's parser whether the statement writes before an ANALYZE (which runs it).
  const requestAnalyze = (kind: 'analyze' | 'rerun') =>
    void statementWrites(sessionId, entry.statement).then((w) => {
      setWrites(w);
      if (w) setConfirm(kind);
      else void runPlan(planId, { analyze: true });
    });
  const saveRaw = () => {
    if (!entry.raw) return;
    void ipc
      .planFileSave(entry.raw)
      .then((path) => path && toast.success(t('toolbar.saved', { path })))
      .catch((e) => toast.error(toIpcError(e).message));
  };
  const copyRaw = () => {
    if (!entry.raw) return;
    void navigator.clipboard
      ?.writeText(entry.raw.text)
      .then(() => toast.success(t('toolbar.copied')));
  };
  const secondary: {
    key: string;
    label: string;
    icon: LucideIcon;
    disabled: boolean;
    onClick: () => void;
  }[] = [
    {
      key: 'save',
      label: t('toolbar.save'),
      icon: FileDown,
      disabled: !entry.raw,
      onClick: saveRaw,
    },
    {
      key: 'open',
      label: t('toolbar.open'),
      icon: FolderOpen,
      disabled: false,
      onClick: () => void openPlanFile({ sessionId, connectionId }),
    },
    {
      key: 'compare',
      label: t('compare.open'),
      icon: GitCompare,
      disabled: !tree,
      onClick: () => setComparing(true),
    },
    {
      key: 'copyRaw',
      label: t('toolbar.copyRaw'),
      icon: Copy,
      disabled: !entry.raw,
      onClick: copyRaw,
    },
  ];
  const showStats = width >= STATS_MIN_PX;
  const header = (
    <div
      className="flex h-9 shrink-0 flex-nowrap items-center gap-x-3 overflow-hidden border-b border-border px-3 text-xs"
      data-testid="plan-header"
    >
      <span className="min-w-0 flex-1 truncate font-mono" title={entry.statement}>
        {entry.statement.replace(/\s+/g, ' ')}
      </span>
      {entry.raw && (
        <Badge
          tone={entry.raw.analyzed ? 'success' : 'neutral'}
          className="shrink-0 whitespace-nowrap"
        >
          {entry.raw.analyzed ? t('status.analyzed') : t('status.estimated')}
        </Badge>
      )}
      {entry.raw?.rolledBack && (
        <Badge tone="warning" className="shrink-0 whitespace-nowrap">
          {t('status.rolledBack')}
        </Badge>
      )}
      {entry.raw && showStats && (
        <span className="shrink-0 whitespace-nowrap text-muted-foreground">
          {t(`format.${entry.raw.format}`)}
          {entry.raw.serverVersion
            ? ` · ${t('status.server', { version: entry.raw.serverVersion })}`
            : ''}
        </span>
      )}
      {tree && (
        <span className="flex shrink-0 items-center gap-x-3">
          <Stat label={t('summary.totalCost')} value={formatCost(tree.totals.totalCost)} />
          {showStats && tree.totals.planningTimeMs != null && (
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
          {showStats && <Stat label={t('summary.nodes')} value={formatCount(tree.nodeCount)} />}
        </span>
      )}
      <span className="flex shrink-0 items-center gap-1">
        {entry.request && (
          <>
            <Button
              size="sm"
              disabled={entry.status === 'loading'}
              onClick={() =>
                analyzing ? void runPlan(planId, { analyze: false }) : requestAnalyze('analyze')
              }
            >
              <Gauge size={13} />
              {analyzing ? t('toolbar.estimate') : t('toolbar.analyze')}
            </Button>
            <Button
              size="sm"
              disabled={entry.status === 'loading'}
              onClick={() => (analyzing ? requestAnalyze('rerun') : void runPlan(planId))}
            >
              <RefreshCw size={13} />
              {t('toolbar.rerun')}
            </Button>
            {entry.status === 'loading' && (
              <Button size="sm" variant="danger" onClick={() => cancelPlan(planId)}>
                <X size={13} />
                {t('toolbar.cancel')}
              </Button>
            )}
          </>
        )}
        {width >= MENU_MAX_PX &&
          secondary.map(({ key, label, icon: Icon, disabled, onClick }) =>
            width < ICON_ONLY_MAX_PX ? (
              <IconButton
                key={key}
                label={label}
                icon={<Icon size={13} />}
                disabled={disabled}
                onClick={onClick}
              />
            ) : (
              <Button key={key} size="sm" disabled={disabled} onClick={onClick}>
                <Icon size={13} />
                {label}
              </Button>
            ),
          )}
        {width < MENU_MAX_PX && (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button
                size="icon"
                variant="ghost"
                aria-label={t('toolbar.more')}
                title={t('toolbar.more')}
              >
                <Ellipsis size={14} />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              {secondary.map(({ key, label, icon: Icon, disabled, onClick }) => (
                <DropdownMenuItem key={key} disabled={disabled} onSelect={onClick}>
                  <Icon size={13} />
                  {label}
                </DropdownMenuItem>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
        )}
      </span>
    </div>
  );

  const reconnect = () => {
    setReconnecting(true);
    const stale = entry.sessionId;
    void reconnectSession(stale, entry.connectionId)
      .then((info) => {
        usePlanStore.getState().patch(planId, { sessionId: info.sessionId, leaseId: null });
        void runPlan(planId);
      })
      .catch((e: unknown) => toast.error(toIpcError(e).message))
      .finally(() => setReconnecting(false));
  };

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
        {entry.error && isSessionLost(entry.error) ? (
          <EmptyState
            title={t('status.sessionLost')}
            description={t('status.sessionLostHint')}
            action={
              <Button variant="primary" disabled={reconnecting} onClick={reconnect}>
                {reconnecting ? <Spinner size={12} /> : <PlugZap size={13} />}
                {t('toolbar.reconnect')}
              </Button>
            }
          />
        ) : (
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
        )}
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
                <PlanGraph
                  key={entry.runId}
                  tree={tree}
                  selectedId={entry.selectedId}
                  onSelect={onSelect}
                />
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
          {tree && (
            <PlanInspector
              collapsed={inspectorCollapsed}
              onCollapsedChange={(c) => inspector.choose(c ? 'collapsed' : 'open')}
              width={inspector.width}
              onWidthChange={inspector.resize}
              badge={tree.warnings.length}
            >
              <PlanDetails tree={tree} selected={selected} onSelect={onSelect} />
            </PlanInspector>
          )}
        </div>
      </Tabs>
    );
  }

  return (
    <div ref={rootRef} className="flex h-full min-h-0 flex-col" data-testid="plan-tab">
      {header}
      {body}
      {comparing && <PlanCompare planId={planId} open onOpenChange={setComparing} />}
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
