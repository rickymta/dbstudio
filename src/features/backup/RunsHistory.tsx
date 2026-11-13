import { useQuery } from '@tanstack/react-query';
import { ChevronDown, ChevronRight, FileUp, ShieldCheck } from 'lucide-react';
import { Fragment, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { EmptyState } from '@/components/ui/EmptyState';
import { Input } from '@/components/ui/Input';
import { Select } from '@/components/ui/Select';
import { Spinner } from '@/components/ui/Spinner';
import { toast } from '@/components/ui/toastStore';
import { describeError } from '@/features/connections/errors';
import { ipc, toIpcError } from '@/lib/ipc';
import type { BackupJob, BackupRun } from '@/lib/types';
import { formatBytes, formatDateTime, formatDuration } from './format';
import { StatusBadge } from './JobProgress';
import { backupRunsKey } from './queries';

const ALL = '__all__';
const PAGE = 100;

export interface RunsHistoryProps {
  jobs: BackupJob[];
  /** Open the restore dialog with this file. */
  onRestore: (file: string) => void;
}

/** History of backup / restore / verify runs with a per-job filter. */
export function RunsHistory({ jobs, onRestore }: RunsHistoryProps) {
  const { t, i18n } = useTranslation('backup');
  const [jobId, setJobId] = useState(ALL);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [askVerify, setAskVerify] = useState<BackupRun | null>(null);

  const runs = useQuery({
    queryKey: [...backupRunsKey, jobId],
    queryFn: () => ipc.backupRunsList(jobId === ALL ? null : jobId, PAGE),
  });

  const verify = async (run: BackupRun, passphrase: string | null) => {
    if (!run.filePath) return;
    try {
      await ipc.backupVerify(run.filePath, passphrase);
      toast.info(t('runs.verifyStarted'));
      setAskVerify(null);
    } catch (e) {
      if (toIpcError(e).kind === 'integrity' && !passphrase) setAskVerify(run);
      else toast.error(describeError(t, e));
    }
  };

  const rows = runs.data ?? [];
  return (
    <div className="flex flex-col gap-3 p-3" data-testid="runs-history">
      <div className="flex items-center gap-2">
        <span className="text-xs text-muted-foreground">{t('runs.filterJob')}</span>
        <div className="w-60">
          <Select
            aria-label={t('runs.filterJob')}
            value={jobId}
            onValueChange={setJobId}
            options={[
              { value: ALL, label: t('runs.allJobs') },
              ...jobs.map((j) => ({ value: j.id, label: j.name })),
            ]}
          />
        </div>
        {runs.isFetching && <Spinner size={12} />}
      </div>
      {runs.isError && (
        <p role="alert" className="text-xs text-danger">
          {describeError(t, runs.error)}
        </p>
      )}
      {!runs.isPending && rows.length === 0 && !runs.isError ? (
        <EmptyState title={t('runs.empty')} />
      ) : (
        <table className="w-full border-collapse text-xs">
          <thead>
            <tr className="border-b border-border text-left text-muted-foreground">
              <th className="w-6" />
              <th className="py-1 pr-2">{t('runs.col.started')}</th>
              <th className="pr-2">{t('runs.col.job')}</th>
              <th className="pr-2">{t('runs.col.kind')}</th>
              <th className="pr-2">{t('runs.col.status')}</th>
              <th className="pr-2 text-right">{t('runs.col.size')}</th>
              <th className="pr-2 text-right">{t('runs.col.duration')}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows.map((run) => {
              const open = expanded === run.id;
              const hasFile = !!run.filePath && run.status === 'succeeded' && run.kind === 'backup';
              return (
                <Fragment key={run.id}>
                  <tr className="border-b border-border/50 align-top" data-testid="run-row">
                    <td>
                      <Button
                        size="icon"
                        variant="ghost"
                        aria-label={t('runs.details')}
                        aria-expanded={open}
                        onClick={() => setExpanded(open ? null : run.id)}
                      >
                        {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
                      </Button>
                    </td>
                    <td className="py-1 pr-2 whitespace-nowrap">
                      {formatDateTime(run.startedAt, i18n.language)}
                    </td>
                    <td className="pr-2">{run.jobName}</td>
                    <td className="pr-2">
                      {t(`runs.kind.${run.kind}`)} / {t(`runs.trigger.${run.trigger}`)}
                    </td>
                    <td className="pr-2">
                      <StatusBadge status={run.status} />
                      {run.error && (
                        <p className="mt-0.5 max-w-60 text-danger" title={run.error.message}>
                          {run.error.message}
                        </p>
                      )}
                    </td>
                    <td className="pr-2 text-right">{formatBytes(run.sizeBytes)}</td>
                    <td className="pr-2 text-right">{formatDuration(run.durationMs)}</td>
                    <td className="whitespace-nowrap text-right">
                      {hasFile && (
                        <>
                          <Button size="sm" variant="ghost" onClick={() => void verify(run, null)}>
                            <ShieldCheck size={12} aria-hidden /> {t('runs.verify')}
                          </Button>
                          <Button
                            size="sm"
                            variant="ghost"
                            onClick={() => onRestore(run.filePath!)}
                          >
                            <FileUp size={12} aria-hidden /> {t('runs.restore')}
                          </Button>
                        </>
                      )}
                    </td>
                  </tr>
                  {open && (
                    <tr className="border-b border-border/50 bg-surface-muted/40">
                      <td />
                      <td colSpan={7} className="py-2 pr-2">
                        <dl className="mb-1 grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5">
                          <dt className="text-muted-foreground">{t('runs.engine')}</dt>
                          <dd>{run.engine}</dd>
                          {run.filePath && (
                            <>
                              <dt className="text-muted-foreground">{t('runs.file')}</dt>
                              <dd className="font-mono break-all">{run.filePath}</dd>
                            </>
                          )}
                          {run.objects !== null && (
                            <>
                              <dt className="text-muted-foreground">{t('runs.objects')}</dt>
                              <dd>
                                {run.objects} / {run.rows ?? 0} {t('runs.rows')}
                              </dd>
                            </>
                          )}
                          {run.checksum && (
                            <>
                              <dt className="text-muted-foreground">{t('runs.checksum')}</dt>
                              <dd className="font-mono break-all">{run.checksum}</dd>
                            </>
                          )}
                        </dl>
                        <p className="text-muted-foreground">{t('runs.logTail')}</p>
                        <pre className="max-h-40 overflow-auto rounded bg-surface p-2 text-[11px]">
                          {run.logTail || t('runs.noLog')}
                        </pre>
                      </td>
                    </tr>
                  )}
                </Fragment>
              );
            })}
          </tbody>
        </table>
      )}
      <PassphrasePrompt
        run={askVerify}
        onCancel={() => setAskVerify(null)}
        onSubmit={(run, pass) => void verify(run, pass)}
      />
    </div>
  );
}

function PassphrasePrompt({
  run,
  onCancel,
  onSubmit,
}: {
  run: BackupRun | null;
  onCancel: () => void;
  onSubmit: (run: BackupRun, passphrase: string) => void;
}) {
  const { t } = useTranslation('backup');
  const [pass, setPass] = useState('');
  return (
    <Dialog open={run !== null} onOpenChange={(o) => !o && onCancel()}>
      {run && (
        <DialogContent
          title={t('runs.passphraseTitle')}
          description={t('runs.passphraseDescription')}
          className="w-[min(420px,92vw)]"
          footer={
            <>
              <Button onClick={onCancel}>{t('actions.cancel')}</Button>
              <Button variant="primary" disabled={!pass} onClick={() => onSubmit(run, pass)}>
                {t('runs.verify')}
              </Button>
            </>
          }
        >
          <Input
            type="password"
            aria-label={t('restore.passphrase')}
            autoComplete="off"
            value={pass}
            onChange={(e) => setPass(e.target.value)}
          />
        </DialogContent>
      )}
    </Dialog>
  );
}
