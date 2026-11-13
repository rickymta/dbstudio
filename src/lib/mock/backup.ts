// Mock backup: jobs are kept in memory, running anything is unsupported.

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
  SecretChange,
  Uuid,
} from '../types';
import { clone, ipcError, unsupported } from './util';

let jobs: BackupJob[] = [];
let background: BackgroundSettings = { closeToTray: 'auto', autostart: false };

export const backupMock = {
  async backupJobsList(): Promise<BackupJob[]> {
    return clone(jobs);
  },
  async backupJobSave(job: BackupJob, passphrase: SecretChange): Promise<BackupJob> {
    void passphrase;
    jobs = [...jobs.filter((j) => j.id !== job.id), clone(job)];
    return clone(job);
  },
  async backupJobDelete(id: Uuid): Promise<void> {
    jobs = jobs.filter((j) => j.id !== id);
  },
  async backupJobRunNow(id: Uuid): Promise<JobId> {
    if (!jobs.some((j) => j.id === id)) throw ipcError('notFound', `backup job ${id}`);
    throw unsupported('backup');
  },
  async backupRunManual(
    spec: BackupSpec,
    passphrase: string | null,
    savePassphrase: boolean,
  ): Promise<JobId> {
    void [spec, passphrase, savePassphrase];
    throw unsupported('backup');
  },
  async backupRunsList(
    jobId: Uuid | null,
    limit: number,
    before: string | null,
  ): Promise<BackupRun[]> {
    void [jobId, limit, before];
    return [];
  },
  async backupInspect(path: string, passphrase: string | null): Promise<BackupInspection> {
    void [path, passphrase];
    throw unsupported('backup');
  },
  async backupVerify(path: string, passphrase: string | null): Promise<JobId> {
    void [path, passphrase];
    throw unsupported('backup');
  },
  async restoreStart(
    spec: RestoreSpec,
    passphrase: string | null,
    confirm: ConfirmationGrant | null,
  ): Promise<JobId> {
    void [spec, passphrase, confirm];
    throw unsupported('restore');
  },
  async backupToolsDetect(): Promise<NativeToolInfo[]> {
    return [];
  },
  async backupToolSetPath(tool: NativeTool, path: string | null): Promise<NativeToolInfo> {
    return { tool, path, version: null, source: 'configured', problem: null };
  },
  async schedulePreview(cron: string, timezone: string, count: number): Promise<string[]> {
    void [cron, timezone, count];
    return [];
  },
  async backgroundGet(): Promise<BackgroundSettings> {
    return { ...background };
  },
  async backgroundSet(settings: BackgroundSettings): Promise<BackgroundSettings> {
    background = { ...settings };
    return { ...background };
  },
};

export function resetMockBackup(): void {
  jobs = [];
  background = { closeToTray: 'auto', autostart: false };
}
