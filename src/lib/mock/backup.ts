// Mock backup: jobs and runs live in memory; running a backup / restore / verify emits fake
// `job://progress` events and ends `succeeded`. `schedulePreview` really computes cron times.

import type {
  BackgroundSettings,
  BackupHeader,
  BackupInspection,
  BackupJob,
  BackupRun,
  BackupSpec,
  ConfirmationGrant,
  JobId,
  JobInfo,
  JobKind,
  NativeTool,
  NativeToolInfo,
  RestoreSpec,
  RunKind,
  RunTrigger,
  SecretChange,
  Uuid,
} from '../types';
import { findMockProfile } from './connections';
import { mockEmit } from './events';
import { clone, ipcError, newId } from './util';

/** Passphrase the mock accepts for files whose name contains "enc". */
export const MOCK_PASSPHRASE = 'dbstudio';

const STEP_MS = 30;

let jobs: BackupJob[] = [];
let runs: BackupRun[] = [];
let background: BackgroundSettings = { closeToTray: 'auto', autostart: false };
let configuredTools = new Map<NativeTool, string>();
const confirmTokens = new Map<string, string>();

// ---------------------------------------------------------------------------------------------
// Cron (5 fields: minute hour day-of-month month day-of-week; *, */n, a-b, a-b/n, a,b,c)
// ---------------------------------------------------------------------------------------------

interface CronSpec {
  minute: Set<number>;
  hour: Set<number>;
  dom: Set<number>;
  month: Set<number>;
  dow: Set<number>;
  domStar: boolean;
  dowStar: boolean;
}

const FIELD_RANGES: [number, number][] = [
  [0, 59],
  [0, 23],
  [1, 31],
  [1, 12],
  [0, 7],
];

function parseNumber(text: string, min: number, max: number, field: string): number {
  if (!/^\d+$/.test(text)) throw ipcError('other', `invalid cron ${field}: "${text}"`);
  const n = Number(text);
  if (n < min || n > max) throw ipcError('other', `cron ${field} out of range: ${n}`);
  return n;
}

function parseField(text: string, min: number, max: number, field: string): Set<number> {
  const out = new Set<number>();
  for (const part of text.split(',')) {
    const [base = '', stepText, extra] = part.split('/');
    if (extra !== undefined || base === '')
      throw ipcError('other', `invalid cron ${field}: "${text}"`);
    const step = stepText === undefined ? 1 : parseNumber(stepText, 1, max, field);
    let lo: number;
    let hi: number;
    if (base === '*') {
      lo = min;
      hi = max;
    } else if (base.includes('-')) {
      const [a = '', b = '', more] = base.split('-');
      if (more !== undefined) throw ipcError('other', `invalid cron ${field}: "${text}"`);
      lo = parseNumber(a, min, max, field);
      hi = parseNumber(b, min, max, field);
      if (lo > hi) throw ipcError('other', `invalid cron ${field} range: "${base}"`);
    } else {
      lo = parseNumber(base, min, max, field);
      hi = stepText === undefined ? lo : max;
    }
    for (let v = lo; v <= hi; v += step) out.add(v);
  }
  return out;
}

export function parseCron(cron: string): CronSpec {
  const f = cron.trim().split(/\s+/);
  if (f.length !== 5) throw ipcError('other', 'cron expression must have 5 fields');
  const names = ['minute', 'hour', 'day-of-month', 'month', 'day-of-week'];
  const sets = f.map((text, i) =>
    parseField(text, FIELD_RANGES[i]![0], FIELD_RANGES[i]![1], names[i]!),
  );
  const dow = new Set([...sets[4]!].map((d) => d % 7));
  return {
    minute: sets[0]!,
    hour: sets[1]!,
    dom: sets[2]!,
    month: sets[3]!,
    dow,
    domStar: f[2] === '*',
    dowStar: f[4] === '*',
  };
}

/** The next `count` run times after `from` (RFC 3339, UTC), `local` or `utc` wall clock. */
export function nextCronRuns(
  cron: string,
  timezone: string,
  count: number,
  from: Date = new Date(),
): string[] {
  if (timezone !== 'local' && timezone !== 'utc')
    throw ipcError('other', `unknown timezone: ${timezone}`);
  const utc = timezone === 'utc';
  const spec = parseCron(cron);
  const parts = (d: Date) =>
    utc
      ? {
          y: d.getUTCFullYear(),
          mo: d.getUTCMonth(),
          d: d.getUTCDate(),
          h: d.getUTCHours(),
          mi: d.getUTCMinutes(),
          w: d.getUTCDay(),
        }
      : {
          y: d.getFullYear(),
          mo: d.getMonth(),
          d: d.getDate(),
          h: d.getHours(),
          mi: d.getMinutes(),
          w: d.getDay(),
        };
  const at = (y: number, mo: number, d: number) =>
    utc ? new Date(Date.UTC(y, mo, d)) : new Date(y, mo, d);

  const out: string[] = [];
  let cur = new Date(Math.floor(from.getTime() / 60_000) * 60_000 + 60_000);
  for (let guard = 0; out.length < count && guard < 1_000_000; guard++) {
    const p = parts(cur);
    const domOk = spec.dom.has(p.d);
    const dowOk = spec.dow.has(p.w);
    const dayOk =
      spec.domStar && spec.dowStar
        ? true
        : spec.domStar
          ? dowOk
          : spec.dowStar
            ? domOk
            : domOk || dowOk;
    if (!spec.month.has(p.mo + 1)) cur = at(p.y, p.mo + 1, 1);
    else if (!dayOk) cur = at(p.y, p.mo, p.d + 1);
    else if (!spec.hour.has(p.h)) cur = new Date(cur.getTime() + (60 - p.mi) * 60_000);
    else if (!spec.minute.has(p.mi)) cur = new Date(cur.getTime() + 60_000);
    else {
      out.push(cur.toISOString());
      cur = new Date(cur.getTime() + 60_000);
    }
  }
  return out;
}

// ---------------------------------------------------------------------------------------------
// Fake background jobs
// ---------------------------------------------------------------------------------------------

function jobInfo(kind: JobKind, title: string, connectionId: Uuid | null): JobInfo {
  const now = new Date().toISOString();
  return {
    id: newId(),
    kind,
    title,
    connectionId,
    status: 'running',
    progress: {
      phase: kind === 'verify' ? 'verify' : kind === 'restore' ? 'restore' : 'schema',
      object: null,
      objectsDone: 0,
      objectsTotal: 4,
      rowsDone: 0,
      rowsTotal: null,
      bytesDone: 0,
      bytesTotal: null,
      message: null,
    },
    createdAt: now,
    startedAt: now,
    finishedAt: null,
    error: null,
    result: null,
    cancellable: true,
  };
}

/** Emit started / progress / finished for a fake job; `onFinish` records the outcome. */
function runFakeJob(info: JobInfo, onFinish: (finished: JobInfo) => void): JobId {
  const phases =
    info.kind === 'backup' ? ['schema', 'data', 'compress', 'encrypt'] : [info.progress.phase];
  mockEmit('job://progress', { type: 'started', job: clone(info) });
  const total = info.progress.objectsTotal ?? 4;
  for (let step = 1; step <= total; step++) {
    setTimeout(() => {
      mockEmit('job://progress', {
        type: 'progress',
        jobId: info.id,
        progress: {
          ...info.progress,
          phase: phases[Math.min(step - 1, phases.length - 1)]!,
          object: `object_${step}`,
          objectsDone: step,
          rowsDone: step * 2500,
          bytesDone: step * 1_048_576,
        },
      });
    }, step * STEP_MS);
  }
  setTimeout(
    () => {
      const done: JobInfo = {
        ...clone(info),
        status: 'succeeded',
        finishedAt: new Date().toISOString(),
        progress: { ...info.progress, objectsDone: total, rowsDone: total * 2500 },
        cancellable: false,
      };
      onFinish(done);
      mockEmit('job://progress', { type: 'finished', job: clone(done) });
      mockEmit('backup://changed', null);
    },
    (total + 1) * STEP_MS,
  );
  return info.id;
}

function startRun(opts: {
  kind: RunKind;
  trigger: RunTrigger;
  jobId: Uuid | null;
  jobName: string;
  connectionId: Uuid | null;
  engine: string;
  filePath: string | null;
  onSuccess?: () => void;
}): JobId {
  const info = jobInfo(opts.kind, opts.jobName, opts.connectionId);
  const run: BackupRun = {
    id: info.id,
    jobId: opts.jobId,
    jobName: opts.jobName,
    connectionId: opts.connectionId,
    kind: opts.kind,
    trigger: opts.trigger,
    engine: opts.engine,
    status: 'running',
    startedAt: info.startedAt!,
    finishedAt: null,
    durationMs: null,
    filePath: opts.kind === 'backup' ? opts.filePath : null,
    sizeBytes: null,
    checksum: null,
    objects: null,
    rows: null,
    error: null,
    logTail: `${opts.kind} started\n`,
  };
  runs = [run, ...runs];
  return runFakeJob(info, (done) => {
    run.status = 'succeeded';
    run.finishedAt = done.finishedAt;
    run.durationMs = (STEP_MS * ((info.progress.objectsTotal ?? 4) + 1)) | 0;
    run.objects = done.progress.objectsDone;
    run.rows = done.progress.rowsDone;
    if (opts.kind === 'backup') {
      run.sizeBytes = done.progress.objectsDone * 1_048_576;
      run.checksum = 'sha256:' + 'ab12cd34'.repeat(8);
    }
    run.logTail += `${opts.kind} finished\n`;
    opts.onSuccess?.();
  });
}

function fileNameFor(spec: BackupSpec, jobName: string): string {
  const profile = findMockProfile(spec.connectionId);
  const db = spec.targets[0]?.segments[0]?.name ?? profile?.database ?? 'db';
  const stamp = new Date().toISOString().replace(/[-:T]/g, '').slice(0, 14);
  const dir = spec.destination.path.endsWith('/')
    ? spec.destination.path.slice(0, -1)
    : spec.destination.path;
  return `${dir}/${(profile?.name ?? 'connection').replace(/\s+/g, '_')}-${db}-${stamp}-${jobName.replace(/\s+/g, '_')}.dbsbak`;
}

function engineName(spec: BackupSpec): string {
  return spec.engine.kind === 'native' ? spec.engine.tool : 'logical';
}

function checkPassphrase(path: string, passphrase: string | null): void {
  if (!path.includes('enc')) return;
  if (passphrase !== MOCK_PASSPHRASE) {
    throw ipcError(
      'integrity',
      passphrase ? 'wrong passphrase or corrupted file' : 'passphrase required',
    );
  }
}

function nextRunOf(job: BackupJob): string | null {
  if (!job.enabled || !job.schedule) return null;
  try {
    return nextCronRuns(job.schedule.cron, job.schedule.timezone, 1)[0] ?? null;
  } catch {
    return null;
  }
}

const TOOL_DEFAULTS: NativeToolInfo[] = [
  {
    tool: 'pgDump',
    path: '/usr/local/bin/pg_dump',
    version: 'pg_dump (PostgreSQL) 16.4',
    source: 'path',
    problem: null,
  },
  {
    tool: 'pgRestore',
    path: '/usr/local/bin/pg_restore',
    version: 'pg_restore (PostgreSQL) 16.4',
    source: 'path',
    problem: null,
  },
  {
    tool: 'psql',
    path: '/usr/local/bin/psql',
    version: 'psql (PostgreSQL) 16.4',
    source: 'knownDir',
    problem: null,
  },
  { tool: 'mySqlDump', path: null, version: null, source: 'path', problem: 'not found' },
  { tool: 'mySqlClient', path: null, version: null, source: 'path', problem: 'not found' },
  { tool: 'mongoDump', path: null, version: null, source: 'path', problem: 'not found' },
];

function toolInfo(tool: NativeTool): NativeToolInfo {
  const configured = configuredTools.get(tool);
  if (configured) {
    return { tool, path: configured, version: 'custom', source: 'configured', problem: null };
  }
  return (
    TOOL_DEFAULTS.find((t) => t.tool === tool) ?? {
      tool,
      path: null,
      version: null,
      source: 'path',
      problem: 'not found',
    }
  );
}

function mockHeader(encrypted: boolean): BackupHeader {
  return {
    formatVersion: 1,
    compression: 'gzip',
    encryption: encrypted
      ? {
          alg: 'aes-256-gcm-chunked',
          kdf: 'argon2id',
          salt: '00',
          m: 65536,
          t: 3,
          p: 1,
          noncePrefix: '00',
          chunkSize: 1048576,
        }
      : null,
    createdAt: '2026-11-01T02:00:00Z',
  };
}

export const backupMock = {
  async backupJobsList(): Promise<BackupJob[]> {
    return clone([...jobs].sort((a, b) => a.name.localeCompare(b.name)));
  },
  async backupJobSave(job: BackupJob, passphrase: SecretChange): Promise<BackupJob> {
    void passphrase;
    const saved: BackupJob = { ...clone(job), nextRunAt: nextRunOf(job) };
    jobs = [...jobs.filter((j) => j.id !== job.id), saved];
    return clone(saved);
  },
  async backupJobDelete(id: Uuid): Promise<void> {
    // Files already written stay on disk; only the definition goes away.
    jobs = jobs.filter((j) => j.id !== id);
  },
  async backupJobRunNow(id: Uuid): Promise<JobId> {
    const job = jobs.find((j) => j.id === id);
    if (!job) throw ipcError('notFound', `backup job ${id}`);
    return startRun({
      kind: 'backup',
      trigger: 'manual',
      jobId: job.id,
      jobName: job.name,
      connectionId: job.spec.connectionId,
      engine: engineName(job.spec),
      filePath: fileNameFor(job.spec, job.name),
      onSuccess: () => {
        job.lastRunAt = new Date().toISOString();
        job.nextRunAt = nextRunOf(job);
      },
    });
  },
  async backupRunManual(
    spec: BackupSpec,
    passphrase: string | null,
    savePassphrase: boolean,
  ): Promise<JobId> {
    void [passphrase, savePassphrase];
    if (!findMockProfile(spec.connectionId))
      throw ipcError('notFound', `connection ${spec.connectionId}`);
    return startRun({
      kind: 'backup',
      trigger: 'manual',
      jobId: null,
      jobName: 'Manual backup',
      connectionId: spec.connectionId,
      engine: engineName(spec),
      filePath: fileNameFor(spec, 'manual'),
    });
  },
  async backupRunsList(
    jobId: Uuid | null,
    limit: number,
    before: string | null,
  ): Promise<BackupRun[]> {
    return clone(
      runs
        .filter((r) => (jobId ? r.jobId === jobId : true))
        .filter((r) => (before ? r.startedAt < before : true))
        .sort((a, b) => b.startedAt.localeCompare(a.startedAt))
        .slice(0, limit),
    );
  },
  async backupInspect(path: string, passphrase: string | null): Promise<BackupInspection> {
    const encrypted = path.includes('enc');
    if (encrypted && passphrase === null) {
      return {
        header: mockHeader(true),
        preamble: null,
        encrypted: true,
        native: null,
      };
    }
    checkPassphrase(path, passphrase);
    return {
      header: mockHeader(encrypted),
      preamble: {
        formatVersion: 2,
        engine: 'logical',
        createdAt: '2026-11-01T02:00:00Z',
        database: 'shop',
        source: {
          kind: 'mysql',
          host: 'staging.example',
          database: 'shop',
          serverVersion: '8.4.0',
        },
        dialect: 'mysql',
        content: 'both',
        objects: [
          { name: 'shop.customers', kind: 'table' },
          { name: 'shop.orders', kind: 'table' },
          { name: 'shop.order_items', kind: 'table' },
          { name: 'shop.v_revenue', kind: 'view' },
        ],
        tables: 3,
        unsupported: ['roles and grants', 'server level settings'],
      },
      encrypted,
      native: null,
    };
  },
  async backupVerify(path: string, passphrase: string | null): Promise<JobId> {
    checkPassphrase(path, passphrase);
    return startRun({
      kind: 'verify',
      trigger: 'manual',
      jobId: null,
      jobName: path.split('/').pop() ?? path,
      connectionId: null,
      engine: 'logical',
      filePath: null,
    });
  },
  async restoreStart(
    spec: RestoreSpec,
    passphrase: string | null,
    confirm: ConfirmationGrant | null,
  ): Promise<JobId> {
    checkPassphrase(spec.file, passphrase);
    const profile = findMockProfile(spec.targetConnectionId);
    if (!profile) throw ipcError('notFound', `connection ${spec.targetConnectionId}`);
    if (profile.readOnly) throw ipcError('readOnly', `connection ${profile.name} is read-only`);
    if (profile.environment === 'production' || spec.clean) {
      const expected = confirmTokens.get(confirm?.token ?? '');
      const confirmText = spec.targetDatabase || profile.database || profile.name;
      if (!confirm || expected === undefined || confirm.typed !== expected) {
        const token = newId();
        confirmTokens.set(token, confirmText);
        throw ipcError('confirmationRequired', 'restore needs confirmation', {
          confirmation: { token, confirmText, statements: [], expiresInMs: 120_000 },
        });
      }
      confirmTokens.delete(confirm.token);
    }
    return startRun({
      kind: 'restore',
      trigger: 'manual',
      jobId: null,
      jobName: spec.file.split('/').pop() ?? spec.file,
      connectionId: profile.id,
      engine: 'logical',
      filePath: null,
    });
  },
  async backupToolsDetect(): Promise<NativeToolInfo[]> {
    return TOOL_DEFAULTS.map((t) => toolInfo(t.tool));
  },
  async backupToolSetPath(tool: NativeTool, path: string | null): Promise<NativeToolInfo> {
    if (path) configuredTools.set(tool, path);
    else configuredTools.delete(tool);
    return toolInfo(tool);
  },
  async schedulePreview(cron: string, timezone: string, count: number): Promise<string[]> {
    return nextCronRuns(cron, timezone, count);
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
  runs = [];
  background = { closeToTray: 'auto', autostart: false };
  configuredTools = new Map();
  confirmTokens.clear();
}
