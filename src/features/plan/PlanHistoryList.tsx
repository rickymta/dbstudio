// Plan history (PLAN-07): the plans explained on a connection, reopened as plan tabs.

import { useInfiniteQuery, useQueryClient } from '@tanstack/react-query';
import { Activity, Trash2, TriangleAlert } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { ConfirmDialog } from '@/components/ui/ConfirmDialog';
import { IconButton } from '@/components/ui/IconButton';
import { Spinner } from '@/components/ui/Spinner';
import { toast } from '@/components/ui/toastStore';
import { describeError } from '@/features/connections/errors';
import { ipc } from '@/lib/ipc';
import type { PlanHistoryEntry, Uuid } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { openRawPlan } from './open';

const PAGE = 50;

const planHistoryKey = (connectionId: Uuid | null) => ['planHistory', connectionId] as const;

export function PlanHistoryList({ connectionId }: { connectionId: Uuid | null }) {
  const { t, i18n } = useTranslation('plan');
  const qc = useQueryClient();
  const [confirmClear, setConfirmClear] = useState(false);
  const [opening, setOpening] = useState<Uuid | null>(null);
  const session = useSessionsStore((s) =>
    connectionId ? s.sessionsByConnection[connectionId] : undefined,
  );
  const query = useInfiniteQuery({
    queryKey: planHistoryKey(connectionId),
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) => ipc.planHistoryList(connectionId, PAGE, pageParam),
    getNextPageParam: (last: PlanHistoryEntry[]) =>
      last.length === PAGE ? last[last.length - 1]!.createdAt : undefined,
    // Plans are explained elsewhere (editor, plan tab): read again whenever the list is shown.
    staleTime: 0,
  });
  const entries = query.data?.pages.flat() ?? [];
  const refresh = () => qc.invalidateQueries({ queryKey: ['planHistory'] });
  const df = new Intl.DateTimeFormat(i18n.language, { dateStyle: 'short', timeStyle: 'medium' });
  const nf = new Intl.NumberFormat(i18n.language, { maximumFractionDigits: 2 });
  const fail = (e: unknown) => toast.error(describeError(t, e));

  const open = async (entry: PlanHistoryEntry) => {
    if (!session || opening) return;
    setOpening(entry.id);
    try {
      const record = await ipc.planHistoryGet(entry.id);
      openRawPlan(session, record.rawPlan);
    } catch (e) {
      fail(e);
    } finally {
      setOpening(null);
    }
  };

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="plan-history-list">
      <div className="flex items-center justify-between gap-2 border-b border-border p-2">
        <span className="text-xs text-muted-foreground">
          {session ? t('history.hint') : t('history.notConnected')}
        </span>
        <IconButton
          label={t('history.clear')}
          icon={<Trash2 size={13} />}
          disabled={entries.length === 0}
          onClick={() => setConfirmClear(true)}
        />
      </div>
      <ul className="min-h-0 flex-1 overflow-auto">
        {query.isPending && (
          <li className="flex justify-center p-4">
            <Spinner />
          </li>
        )}
        {query.isError && (
          <li role="alert" className="p-3 text-xs text-danger">
            {describeError(t, query.error)}
          </li>
        )}
        {query.isSuccess && entries.length === 0 && (
          <li className="p-4 text-center text-xs text-muted-foreground">{t('history.empty')}</li>
        )}
        {entries.map((p) => (
          <li
            key={p.id}
            className="group border-b border-border px-2 py-1.5"
            data-testid="plan-history-entry"
          >
            <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
              <time dateTime={p.createdAt}>{df.format(new Date(p.createdAt))}</time>
              {p.database && <span className="truncate">{p.database}</span>}
              {p.analyze && (
                <span className="flex items-center gap-0.5 text-accent">
                  <Activity size={11} aria-hidden />
                  {t('history.analyze')}
                </span>
              )}
              {p.warnings > 0 && (
                <span className="flex items-center gap-0.5 text-warning">
                  <TriangleAlert size={11} aria-hidden />
                  {t('history.warnings', { count: p.warnings })}
                </span>
              )}
              <span className="ml-auto flex opacity-0 group-focus-within:opacity-100 group-hover:opacity-100">
                <IconButton
                  label={t('history.delete')}
                  icon={<Trash2 size={12} />}
                  onClick={() => void ipc.planHistoryDelete(p.id).then(refresh, fail)}
                />
              </span>
            </div>
            <button
              type="button"
              disabled={!session || opening !== null}
              title={session ? t('history.open') : t('history.notConnected')}
              onClick={() => void open(p)}
              className="mt-0.5 block w-full rounded text-left hover:bg-surface-muted disabled:cursor-default disabled:hover:bg-transparent"
            >
              <code className="line-clamp-3 font-mono text-xs break-all whitespace-pre-wrap">
                {p.statement}
              </code>
              <span className="mt-0.5 flex gap-2 text-[11px] text-muted-foreground">
                <span>{p.format}</span>
                {p.totalCost !== null && (
                  <span>{t('history.cost', { cost: nf.format(p.totalCost) })}</span>
                )}
                {p.totalMs !== null && <span>{t('history.ms', { ms: nf.format(p.totalMs) })}</span>}
                {opening === p.id && <Spinner size={11} />}
              </span>
            </button>
          </li>
        ))}
        {query.hasNextPage && (
          <li className="flex justify-center p-2">
            <Button
              size="sm"
              disabled={query.isFetchingNextPage}
              onClick={() => void query.fetchNextPage()}
            >
              {t('history.more')}
            </Button>
          </li>
        )}
      </ul>
      <ConfirmDialog
        open={confirmClear}
        onOpenChange={setConfirmClear}
        title={t('history.clearTitle')}
        description={t('history.clearDescription')}
        confirmLabel={t('history.clear')}
        destructive
        onConfirm={() => {
          setConfirmClear(false);
          void ipc.planHistoryClear(connectionId).then(refresh, fail);
        }}
      />
    </div>
  );
}
