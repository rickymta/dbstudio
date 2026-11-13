import type {
  ConnectionProfile,
  DbKind,
  Environment,
  SecretChange,
  TlsMode,
  Uuid,
} from '@/lib/types';

export interface ConnectionForm {
  kind: DbKind;
  name: string;
  host: string;
  port: string;
  /** The user typed the port by hand, so changing the database kind must not overwrite it. */
  portTouched: boolean;
  database: string;
  username: string;
  password: string;
  /** Edit mode only, while `password` is empty: keep the stored secret or remove it. */
  passwordAction: 'keep' | 'clear';
  savePassword: boolean;
  /** Empty string = no group. */
  groupId: string;
  /** Empty string = no environment. */
  environment: Environment | '';
  /** Explicit color (CSS hex); `null` = default for the environment. */
  color: string | null;
  readOnly: boolean;
  tlsMode: TlsMode;
  caCertPath: string;
  clientCertPath: string;
  clientKeyPath: string;
  /** Connect timeout in seconds; empty = driver default. */
  timeoutSec: string;
}

export type FormErrors = Partial<Record<'name' | 'host' | 'port' | 'timeout', string>>;

export const DEFAULT_KIND: DbKind = 'postgres';
export const DEFAULT_PORT = 5432;

export function initialForm(
  profile: ConnectionProfile | undefined,
  defaults: { groupId?: Uuid | null } = {},
): ConnectionForm {
  if (!profile) {
    return {
      kind: DEFAULT_KIND,
      name: '',
      host: 'localhost',
      port: String(DEFAULT_PORT),
      portTouched: false,
      database: '',
      username: '',
      password: '',
      passwordAction: 'keep',
      savePassword: true,
      groupId: defaults.groupId ?? '',
      environment: '',
      color: null,
      readOnly: false,
      tlsMode: 'prefer',
      caCertPath: '',
      clientCertPath: '',
      clientKeyPath: '',
      timeoutSec: '',
    };
  }
  return {
    kind: profile.kind,
    name: profile.name,
    host: profile.host,
    port: String(profile.port),
    portTouched: true,
    database: profile.database ?? '',
    username: profile.username ?? '',
    password: '',
    passwordAction: 'keep',
    savePassword: profile.savePassword,
    groupId: profile.groupId ?? '',
    environment: profile.environment ?? '',
    color: profile.color ?? null,
    readOnly: profile.readOnly,
    tlsMode: profile.tls.mode,
    caCertPath: profile.tls.caCertPath ?? '',
    clientCertPath: profile.tls.clientCertPath ?? '',
    clientKeyPath: profile.tls.clientKeyPath ?? '',
    timeoutSec: profile.connectTimeoutMs ? String(Math.round(profile.connectTimeoutMs / 1000)) : '',
  };
}

/** Validation messages are i18n keys under `connections:validation`. */
export function validateForm(form: ConnectionForm): FormErrors {
  const errors: FormErrors = {};
  if (!form.name.trim()) errors.name = 'nameRequired';
  if (!form.host.trim()) errors.host = 'hostRequired';
  const port = Number(form.port);
  if (!/^\d+$/.test(form.port.trim()) || port < 1 || port > 65535) errors.port = 'portRange';
  if (form.timeoutSec.trim() && !/^[1-9]\d*$/.test(form.timeoutSec.trim())) {
    errors.timeout = 'timeoutPositive';
  }
  return errors;
}

const orNull = (s: string): string | null => (s.trim() ? s.trim() : null);

/** Build the profile to save or test. `existing` supplies the fields the form does not edit. */
export function profileFromForm(
  form: ConnectionForm,
  existing: ConnectionProfile | undefined,
  newId: () => Uuid,
): ConnectionProfile {
  const now = new Date().toISOString();
  const verifying = form.tlsMode === 'verifyCa' || form.tlsMode === 'verifyFull';
  return {
    id: existing?.id ?? newId(),
    name: form.name.trim(),
    kind: form.kind,
    host: form.host.trim(),
    port: Number(form.port),
    database: orNull(form.database),
    username: orNull(form.username),
    tls: {
      mode: form.tlsMode,
      caCertPath: verifying ? orNull(form.caCertPath) : null,
      clientCertPath: verifying ? orNull(form.clientCertPath) : null,
      clientKeyPath: verifying ? orNull(form.clientKeyPath) : null,
    },
    connectTimeoutMs: form.timeoutSec.trim() ? Number(form.timeoutSec) * 1000 : null,
    readOnly: form.readOnly,
    environment: form.environment || null,
    options: existing?.options ?? {},
    groupId: form.groupId || null,
    color: form.color,
    savePassword: form.savePassword,
    hasPassword: existing?.hasPassword ?? false,
    sortOrder: existing?.sortOrder ?? 0,
    createdAt: existing?.createdAt ?? now,
    updatedAt: now,
  };
}

/** Secret change to send when saving. */
export function saveSecret(form: ConnectionForm): SecretChange {
  if (!form.savePassword) return { action: 'clear' };
  if (form.password) return { action: 'set', value: form.password };
  return form.passwordAction === 'clear' ? { action: 'clear' } : { action: 'keep' };
}

/** Secret to use for a connection test: whatever was typed, else the stored one. */
export function testSecret(form: ConnectionForm): SecretChange {
  if (form.password) return { action: 'set', value: form.password };
  return form.passwordAction === 'clear' ? { action: 'clear' } : { action: 'keep' };
}
