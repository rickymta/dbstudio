import { useQueryClient } from '@tanstack/react-query';
import { DatabaseBackup, FileUp, Plus, Zap } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { ConfirmDialog } from '@/components/ui/ConfirmDialog';
import { EmptyState } from '@/components/ui/EmptyState';
import { Spinner } from '@/components/ui/Spinner';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/Tabs';
import { toast } from '@/components/ui/toastStore';
import { describeError } from '@/features/connections/errors';
import { useConnectionsSnapshot } from '@/features/connections/queries';
import { ipc } from '@/lib/ipc';
import type { BackupJob } from '@/lib/types';
import { BackupJobDialog } from './BackupJobDialog';
import { JobProgressView } from './JobProgress';
import { JobsList } from './JobsList';
import { NativeToolsPanel } from './NativeToolsPanel';
import { backupJobsKey, isActive, useBackupEvents, useBackupJobsQuery } from './queries';
import { RestoreDialog } from './RestoreDialog';
import { RunsHistory } from './RunsHistory';

type Dialogs =
  | { kind: 'none' }
  | { kind: 'job'; job: BackupJob | null }
  | { kind: 'once' }
  | { kind: 'restore'; file: string | null };

/** Singleton backup tab: jobs, run history and native tools. */
export function BackupTab() {
  const { t } = useTranslation('backup');
  const qc = useQueryClient();
  const jobsQuery = useBackupJobsQuery();
  const connections = useConnectionsSnapshot();
  const profiles = connections.data?.profiles ?? [];
  const events = useBackupEvents();
  const [dialog, setDialog] = useState<Dialogs>({ kind: 'none' });
  const [toDelete, setToDelete] = useState<BackupJob | null>(null);

  const jobs = jobsQuery.data ?? [];
  const running = Object.values(events).filter(isActive);
  const close = () => setDialog({ kind: 'none' });

  const run = async (job: BackupJob) => {
    try {
      await ipc.backupJobRunNow(job.id);
      toast.info(t('jobs.runStarted', { name: job.name }));
    } catch (e) {
      toast.error(describeError(t, e));
    }
  };

  const toggle = async (job: BackupJob, enabled: boolean) => {
    try {
      await ipc.backupJobSave({ ...job, enabled }, { action: 'keep' });
      await qc.invalidateQueries({ queryKey: backupJobsKey });
    } catch (e) {
      toast.error(describeError(t, e));
    }
  };

  const remove = async (job: BackupJob) => {
    try {
      await ipc.backupJobDelete(job.id);
      await qc.invalidateQueries({ queryKey: backupJobsKey });
    } catch (e) {
      toast.error(describeError(t, e));
    }
  };

  return (
    <div className="flex h-full flex-col" data-testid="backup-tab">
      <header className="flex flex-wrap items-center gap-2 border-b border-border px-3 py-2">
        <h2 className="mr-auto flex items-center gap-2 text-sm font-semibold">
          <DatabaseBackup size={16} aria-hidden /> {t('tab.title')}
        </h2>
        <Button onClick={() => setDialog({ kind: 'restore', file: null })}>
          <FileUp size={14} aria-hidden /> {t('actions.restoreFromFile')}
        </Button>
        <Button onClick={() => setDialog({ kind: 'once' })}>
          <Zap size={14} aria-hidden /> {t('actions.backupNow')}
        </Button>
        <Button variant="primary" onClick={() => setDialog({ kind: 'job', job: null })}>
          <Plus size={14} aria-hidden /> {t('actions.createJob')}
        </Button>
      </header>

      {running.length > 0 && (
        <section
          className="flex flex-col gap-2 border-b border-border bg-surface-muted/40 px-3 py-2"
          aria-label={t('running.title')}
          data-testid="running-jobs"
        >
          {running.map((job) => (
            <JobProgressView key={job.id} job={job} />
          ))}
        </section>
      )}

      <Tabs defaultValue="jobs" className="flex min-h-0 flex-1 flex-col">
        <TabsList>
          <TabsTrigger value="jobs">{t('section.jobs')}</TabsTrigger>
          <TabsTrigger value="history">{t('section.history')}</TabsTrigger>
          <TabsTrigger value="tools">{t('section.tools')}</TabsTrigger>
        </TabsList>
        <TabsContent value="jobs" className="overflow-auto p-3">
          {jobsQuery.isPending ? (
            <Spinner />
          ) : jobsQuery.isError ? (
            <p role="alert" className="text-xs text-danger">
              {describeError(t, jobsQuery.error)}
            </p>
          ) : jobs.length === 0 ? (
            <EmptyState
              icon={<DatabaseBackup size={32} />}
              title={t('jobs.empty.title')}
              description={t('jobs.empty.description')}
              action={
                <Button variant="primary" onClick={() => setDialog({ kind: 'job', job: null })}>
                  <Plus size={14} aria-hidden /> {t('actions.createJob')}
                </Button>
              }
            />
          ) : (
            <JobsList
              jobs={jobs}
              profiles={profiles}
              loadingProfiles={connections.isPending}
              onRun={(j) => void run(j)}
              onEdit={(j) => setDialog({ kind: 'job', job: j })}
              onDelete={setToDelete}
              onToggle={(j, v) => void toggle(j, v)}
            />
          )}
        </TabsContent>
        <TabsContent value="history" className="overflow-auto">
          <RunsHistory jobs={jobs} onRestore={(file) => setDialog({ kind: 'restore', file })} />
        </TabsContent>
        <TabsContent value="tools" className="overflow-auto">
          <NativeToolsPanel />
        </TabsContent>
      </Tabs>

      {dialog.kind === 'job' && (
        <BackupJobDialog open onOpenChange={(o) => !o && close()} job={dialog.job} />
      )}
      {dialog.kind === 'once' && (
        <BackupJobDialog open onOpenChange={(o) => !o && close()} mode="once" />
      )}
      {dialog.kind === 'restore' && (
        <RestoreDialog open onOpenChange={(o) => !o && close()} initialFile={dialog.file} />
      )}
      <ConfirmDialog
        open={toDelete !== null}
        onOpenChange={(o) => !o && setToDelete(null)}
        title={t('jobs.deleteConfirm.title', { name: toDelete?.name ?? '' })}
        description={t('jobs.deleteConfirm.description')}
        confirmLabel={t('actions.delete')}
        cancelLabel={t('actions.cancel')}
        destructive
        onConfirm={() => toDelete && void remove(toDelete)}
      />
    </div>
  );
}
