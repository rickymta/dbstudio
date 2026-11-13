// Pure logic of the backup job form: defaults, file name example, validation, payload building.

import type {
  BackupEngine,
  BackupJob,
  BackupSpec,
  Compression,
  ConnectionProfile,
  ExportContent,
  MissedRunPolicy,
  NativeTool,
  ObjectPath,
  ScheduleTz,
  SecretChange,
  DbKind,
} from '@/lib/types';
import { buildCron, DEFAULT_PRESET } from './schedule';
import { isCronShape } from './schedule';

export const DEFAULT_FILE_TEMPLATE = '{connection}-{database}-{date:%Y%m%d-%H%M%S}';
/** Extension the backend gives backup files. */
export const BACKUP_EXTENSION = '.dbsbak';

export type CompressionKind = 'none' | 'gzip' | 'zstd';
export const COMPRESSION_LEVELS: Record<
  'gzip' | 'zstd',
  { min: number; max: number; def: number }
> = {
  gzip: { min: 1, max: 9, def: 6 },
  zstd: { min: 1, max: 19, def: 3 },
};

/** Native tool used for each database kind (first = default). */
export const NATIVE_TOOLS: Partial<Record<DbKind, NativeTool>> = {
  postgres: 'pgDump',
  mysql: 'mySqlDump',
  mongodb: 'mongoDump',
  oracle: 'expdp',
  redis: 'redisCli',
  sqlserver: 'msSqlServerBackup',
};

/** Native tools that write to a directory on the database server. */
export const SERVER_SIDE_TOOLS: readonly NativeTool[] = ['expdp', 'msSqlServerBackup'];

export interface JobForm {
  name: string;
  enabled: boolean;
  connectionId: string;
  database: string;
  directory: string;
  fileTemplate: string;
  engineKind: 'logical' | 'native';
  content: ExportContent;
  nativeTool: NativeTool | '';
  compression: CompressionKind;
  level: number;
  encrypted: boolean;
  passphrase: string;
  passphrase2: string;
  scheduled: boolean;
  cron: string;
  timezone: ScheduleTz;
  missedRun: MissedRunPolicy;
  keepLast: string;
  maxAgeDays: string;
  notifyOnSuccess: boolean;
  notifyOnFailure: boolean;
}

export function emptyForm(): JobForm {
  return {
    name: '',
    enabled: true,
    connectionId: '',
    database: '',
    directory: '',
    fileTemplate: DEFAULT_FILE_TEMPLATE,
    engineKind: 'logical',
    content: 'both',
    nativeTool: '',
    compression: 'gzip',
    level: COMPRESSION_LEVELS.gzip.def,
    encrypted: false,
    passphrase: '',
    passphrase2: '',
    scheduled: true,
    cron: buildCron(DEFAULT_PRESET),
    timezone: 'local',
    missedRun: 'runOnce',
    keepLast: '',
    maxAgeDays: '',
    notifyOnSuccess: false,
    notifyOnFailure: true,
  };
}

function two(n: number): string {
  return String(n).padStart(2, '0');
}

/** Expand `%Y %m %d %H %M %S` (UTC-free: uses the given date's local fields). */
export function formatDate(pattern: string, date: Date): string {
  return pattern
    .replace(/%Y/g, String(date.getFullYear()))
    .replace(/%m/g, two(date.getMonth() + 1))
    .replace(/%d/g, two(date.getDate()))
    .replace(/%H/g, two(date.getHours()))
    .replace(/%M/g, two(date.getMinutes()))
    .replace(/%S/g, two(date.getSeconds()));
}

export interface TemplateValues {
  connection: string;
  database: string;
  job: string;
}

const PLACEHOLDER = /\{([a-zA-Z]+)(?::([^}]*))?\}/g;
const KNOWN_PLACEHOLDERS = new Set(['connection', 'database', 'job', 'date']);

/** Placeholder names in a template that the backend does not know. */
export function unknownPlaceholders(template: string): string[] {
  const out: string[] = [];
  for (const m of template.matchAll(PLACEHOLDER)) {
    if (!KNOWN_PLACEHOLDERS.has(m[1]!)) out.push(m[1]!);
  }
  return out;
}

function sanitizeSegment(text: string): string {
  return text.replace(/[\\/:*?"<>|\s]+/g, '_');
}

/** Example file name for a template (the extension is added by the backend). */
export function fileNameExample(
  template: string,
  values: TemplateValues,
  now: Date = new Date(),
): string {
  const body = template.replace(PLACEHOLDER, (whole, name: string, arg?: string) => {
    switch (name) {
      case 'connection':
        return sanitizeSegment(values.connection || 'connection');
      case 'database':
        return sanitizeSegment(values.database || 'database');
      case 'job':
        return sanitizeSegment(values.job || 'job');
      case 'date':
        return formatDate(arg || '%Y%m%d-%H%M%S', now);
      default:
        return whole;
    }
  });
  return `${body}${BACKUP_EXTENSION}`;
}

export type JobFormField =
  | 'name'
  | 'connection'
  | 'database'
  | 'directory'
  | 'fileTemplate'
  | 'nativeTool'
  | 'passphrase'
  | 'cron'
  | 'keepLast'
  | 'maxAgeDays';

/** Section (tab) a field lives in. */
export const FIELD_SECTION: Record<
  JobFormField,
  'source' | 'destination' | 'format' | 'schedule' | 'policy'
> = {
  name: 'source',
  connection: 'source',
  database: 'source',
  directory: 'destination',
  fileTemplate: 'destination',
  nativeTool: 'format',
  passphrase: 'format',
  cron: 'schedule',
  keepLast: 'policy',
  maxAgeDays: 'policy',
};

export type FormErrors = Partial<Record<JobFormField, string>>;

export interface ValidateOptions {
  /** One-off backup: no name, schedule or retention. */
  once?: boolean;
  /** The job being edited was already encrypted (a stored passphrase can be kept). */
  hadPassphrase?: boolean;
  /** Tools reported by `backup_tools_detect`, to check the selected native tool. */
  toolUsable?: (tool: NativeTool) => boolean;
  /** The selected connection needs no database (e.g. Redis). */
  databaseOptional?: boolean;
}

function parsePositive(text: string): number | null {
  if (!/^\d+$/.test(text.trim())) return null;
  const n = Number(text);
  return n >= 1 && n <= 100_000 ? n : null;
}

/** Validation error codes (suffix of the `backup:job.error.*` keys), per field. */
export function validateForm(form: JobForm, opts: ValidateOptions = {}): FormErrors {
  const e: FormErrors = {};
  if (!opts.once && !form.name.trim()) e.name = 'nameRequired';
  if (!form.connectionId) e.connection = 'connectionRequired';
  if (!opts.databaseOptional && !form.database.trim()) e.database = 'databaseRequired';
  if (!form.directory.trim()) e.directory = 'directoryRequired';
  if (/[\\/]/.test(form.fileTemplate)) e.fileTemplate = 'templateSeparator';
  else if (!form.fileTemplate.trim()) e.fileTemplate = 'templateRequired';
  else if (unknownPlaceholders(form.fileTemplate).length) e.fileTemplate = 'templateUnknown';
  if (form.engineKind === 'native') {
    if (!form.nativeTool) e.nativeTool = 'toolRequired';
    else if (opts.toolUsable && !opts.toolUsable(form.nativeTool)) e.nativeTool = 'toolMissing';
  }
  if (form.encrypted) {
    const keepStored = !!opts.hadPassphrase && !form.passphrase && !form.passphrase2;
    if (!keepStored) {
      if (form.passphrase.length < 1) e.passphrase = 'passphraseRequired';
      else if (form.passphrase !== form.passphrase2) e.passphrase = 'passphraseMismatch';
    }
  }
  if (!opts.once) {
    if (form.scheduled && !isCronShape(form.cron)) e.cron = 'cronInvalid';
    if (form.keepLast.trim() && parsePositive(form.keepLast) === null)
      e.keepLast = 'retentionInvalid';
    if (form.maxAgeDays.trim() && parsePositive(form.maxAgeDays) === null)
      e.maxAgeDays = 'retentionInvalid';
  }
  return e;
}

/** First section (in tab order) containing an error, or `null`. */
export function firstErrorSection(errors: FormErrors): string | null {
  const order = ['source', 'destination', 'format', 'schedule', 'policy'];
  const sections = new Set(Object.keys(errors).map((k) => FIELD_SECTION[k as JobFormField]));
  return order.find((s) => sections.has(s as never)) ?? null;
}

export function buildCompression(form: JobForm): Compression {
  if (form.compression === 'none') return { kind: 'none' };
  const range = COMPRESSION_LEVELS[form.compression];
  const level = Math.min(range.max, Math.max(range.min, Math.round(form.level)));
  return { kind: form.compression, level };
}

export function buildTargets(form: JobForm): ObjectPath[] {
  const db = form.database.trim();
  return db ? [{ segments: [{ kind: 'database', name: db }] }] : [];
}

/** Options sent for each native tool: complete backups by default (the backend allow-lists keys). */
export const NATIVE_DEFAULT_OPTIONS: Partial<Record<NativeTool, Record<string, string>>> = {
  mySqlDump: { singleTransaction: 'true', routines: 'true', triggers: 'true', events: 'true' },
  pgDump: { format: 'custom' },
};

export function buildSpec(form: JobForm): BackupSpec {
  const engine: BackupEngine =
    form.engineKind === 'logical'
      ? { kind: 'logical', content: form.content }
      : {
          kind: 'native',
          tool: form.nativeTool as NativeTool,
          options: NATIVE_DEFAULT_OPTIONS[form.nativeTool as NativeTool] ?? {},
        };
  const path = form.directory.trim();
  const serverSide =
    form.engineKind === 'native' && SERVER_SIDE_TOOLS.includes(form.nativeTool as NativeTool);
  return {
    connectionId: form.connectionId,
    targets: buildTargets(form),
    engine,
    compression: buildCompression(form),
    encrypted: form.encrypted,
    destination: serverSide
      ? { kind: 'serverDir', path }
      : { kind: 'localDir', path, fileTemplate: form.fileTemplate.trim() },
  };
}

/** Passphrase change to send with `backup_job_save`. */
export function passphraseChange(form: JobForm): SecretChange {
  if (!form.encrypted) return { action: 'clear' };
  return form.passphrase ? { action: 'set', value: form.passphrase } : { action: 'keep' };
}

function optionalNumber(text: string): number | null {
  return text.trim() ? Number(text) : null;
}

/** Build the job to save from a validated form; `existing` keeps id, run times and timestamps. */
export function buildJob(
  form: JobForm,
  existing: BackupJob | null,
  id: string,
  now: string,
): BackupJob {
  const keepLast = optionalNumber(form.keepLast);
  const maxAgeDays = optionalNumber(form.maxAgeDays);
  return {
    id: existing?.id ?? id,
    name: form.name.trim(),
    enabled: form.enabled,
    spec: buildSpec(form),
    schedule: form.scheduled
      ? { cron: form.cron.trim().replace(/\s+/g, ' '), timezone: form.timezone }
      : null,
    retention: { keepLast, maxAgeDays },
    missedRun: form.missedRun,
    notify: { onSuccess: form.notifyOnSuccess, onFailure: form.notifyOnFailure },
    lastRunAt: existing?.lastRunAt ?? null,
    nextRunAt: existing?.nextRunAt ?? null,
    createdAt: existing?.createdAt ?? now,
    updatedAt: now,
  };
}

/** Inverse of `buildJob` for the edit dialog. */
export function formFromJob(job: BackupJob, profile?: ConnectionProfile): JobForm {
  const base = emptyForm();
  const spec = job.spec;
  const comp = spec.compression ?? { kind: 'none' as const };
  const dest = spec.destination;
  const database =
    spec.targets[0]?.segments.find((s) => s.kind === 'database')?.name ?? profile?.database ?? '';
  return {
    ...base,
    name: job.name,
    enabled: job.enabled,
    connectionId: spec.connectionId,
    database,
    directory: dest.path,
    fileTemplate:
      dest.kind === 'localDir'
        ? (dest.fileTemplate ?? DEFAULT_FILE_TEMPLATE)
        : DEFAULT_FILE_TEMPLATE,
    engineKind: spec.engine.kind,
    content: spec.engine.kind === 'logical' ? (spec.engine.content ?? 'both') : 'both',
    nativeTool: spec.engine.kind === 'native' ? spec.engine.tool : '',
    compression: comp.kind,
    level: comp.kind === 'none' ? COMPRESSION_LEVELS.gzip.def : comp.level,
    encrypted: !!spec.encrypted,
    scheduled: job.schedule !== null,
    cron: job.schedule?.cron ?? base.cron,
    timezone: job.schedule?.timezone ?? 'local',
    missedRun: job.missedRun ?? 'runOnce',
    keepLast: job.retention?.keepLast ? String(job.retention.keepLast) : '',
    maxAgeDays: job.retention?.maxAgeDays ? String(job.retention.maxAgeDays) : '',
    notifyOnSuccess: job.notify?.onSuccess ?? false,
    notifyOnFailure: job.notify?.onFailure ?? true,
  };
}
