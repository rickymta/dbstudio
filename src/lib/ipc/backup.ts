import { mock } from '../mock';
import type {
  BackgroundSettings,
  BackupInspection,
  BackupJob,
  BackupRun,
  BackupSpec,
  ConfirmationGrant,
  JobId,
  NativeTool,
  NativeToolInfo,
  RestoreSpec,
  Secret,
  SecretChange,
  Uuid,
} from '../types';
import { call } from './core';

/** Backup jobs, runs, restore, native tools and background behavior. */
export const backupApi = {
  backupJobsList: (): Promise<BackupJob[]> => call('backup_jobs_list', () => mock.backupJobsList()),
  backupJobSave: (job: BackupJob, passphrase: SecretChange): Promise<BackupJob> =>
    call('backup_job_save', () => mock.backupJobSave(job, passphrase), { job, passphrase }),
  backupJobDelete: (id: Uuid): Promise<void> =>
    call('backup_job_delete', () => mock.backupJobDelete(id), { id }),
  backupJobRunNow: (id: Uuid): Promise<JobId> =>
    call('backup_job_run_now', () => mock.backupJobRunNow(id), { id }),
  backupRunManual: (
    spec: BackupSpec,
    passphrase?: Secret | null,
    savePassphrase?: boolean,
  ): Promise<JobId> =>
    call(
      'backup_run_manual',
      () => mock.backupRunManual(spec, passphrase ?? null, savePassphrase ?? false),
      { spec, passphrase: passphrase ?? null, savePassphrase: savePassphrase ?? false },
    ),
  backupRunsList: (
    jobId: Uuid | null | undefined,
    limit: number,
    before?: string | null,
  ): Promise<BackupRun[]> =>
    call('backup_runs_list', () => mock.backupRunsList(jobId ?? null, limit, before ?? null), {
      jobId: jobId ?? null,
      limit,
      before: before ?? null,
    }),
  backupInspect: (path: string, passphrase?: Secret | null): Promise<BackupInspection> =>
    call('backup_inspect', () => mock.backupInspect(path, passphrase ?? null), {
      path,
      passphrase: passphrase ?? null,
    }),
  backupVerify: (path: string, passphrase?: Secret | null): Promise<JobId> =>
    call('backup_verify', () => mock.backupVerify(path, passphrase ?? null), {
      path,
      passphrase: passphrase ?? null,
    }),
  restoreStart: (
    spec: RestoreSpec,
    passphrase?: Secret | null,
    confirm?: ConfirmationGrant | null,
  ): Promise<JobId> =>
    call('restore_start', () => mock.restoreStart(spec, passphrase ?? null, confirm ?? null), {
      spec,
      passphrase: passphrase ?? null,
      confirm: confirm ?? null,
    }),
  backupToolsDetect: (): Promise<NativeToolInfo[]> =>
    call('backup_tools_detect', () => mock.backupToolsDetect()),
  backupToolSetPath: (tool: NativeTool, path: string | null): Promise<NativeToolInfo> =>
    call('backup_tool_set_path', () => mock.backupToolSetPath(tool, path), { tool, path }),
  /** Next run times (RFC 3339); rejects with `other` when the cron expression is invalid. */
  schedulePreview: (cron: string, timezone: string, count: number): Promise<string[]> =>
    call('schedule_preview', () => mock.schedulePreview(cron, timezone, count), {
      cron,
      timezone,
      count,
    }),
  backgroundGet: (): Promise<BackgroundSettings> =>
    call('background_get', () => mock.backgroundGet()),
  backgroundSet: (settings: BackgroundSettings): Promise<BackgroundSettings> =>
    call('background_set', () => mock.backgroundSet(settings), { settings }),
};
