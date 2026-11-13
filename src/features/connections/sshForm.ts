import type { ConnectionProfile, SecretChange, SshConfig } from '@/lib/types';

export type SshAuthMethod = 'password' | 'privateKey' | 'agent';

/** Editable state of the SSH tunnel part of the connection dialog. */
export interface SshFormState {
  enabled: boolean;
  host: string;
  port: string;
  username: string;
  method: SshAuthMethod;
  keyPath: string;
  /** The private key file is encrypted with a passphrase. */
  hasPassphrase: boolean;
  /** Password or key passphrase typed in this session. */
  secret: string;
  /** Edit mode while `secret` is empty: keep the stored secret or remove it. */
  secretAction: 'keep' | 'clear';
  /** Keep-alive interval in seconds; empty = none. */
  keepaliveSec: string;
}

export const DEFAULT_SSH_PORT = 22;

export type SshErrors = Partial<
  Record<'host' | 'port' | 'username' | 'keyPath' | 'keepalive', string>
>;

export function initialSshForm(profile?: ConnectionProfile): SshFormState {
  const ssh = profile?.ssh;
  if (!ssh) {
    return {
      enabled: false,
      host: '',
      port: String(DEFAULT_SSH_PORT),
      username: '',
      method: 'password',
      keyPath: '',
      hasPassphrase: false,
      secret: '',
      secretAction: 'keep',
      keepaliveSec: '',
    };
  }
  return {
    enabled: true,
    host: ssh.host,
    port: String(ssh.port),
    username: ssh.username,
    method: ssh.auth.method,
    keyPath: ssh.auth.method === 'privateKey' ? ssh.auth.keyPath : '',
    hasPassphrase: ssh.auth.method === 'privateKey' ? ssh.auth.hasPassphrase : false,
    secret: '',
    secretAction: 'keep',
    keepaliveSec: ssh.keepaliveSecs ? String(ssh.keepaliveSecs) : '',
  };
}

/** Validation messages are i18n keys under `ssh:validation`. Nothing is validated while off. */
export function validateSsh(s: SshFormState): SshErrors {
  const errors: SshErrors = {};
  if (!s.enabled) return errors;
  if (!s.host.trim()) errors.host = 'hostRequired';
  const port = Number(s.port);
  if (!/^\d+$/.test(s.port.trim()) || port < 1 || port > 65535) errors.port = 'portRange';
  if (!s.username.trim()) errors.username = 'usernameRequired';
  if (s.method === 'privateKey' && !s.keyPath.trim()) errors.keyPath = 'keyRequired';
  if (s.keepaliveSec.trim() && !/^[1-9]\d*$/.test(s.keepaliveSec.trim())) {
    errors.keepalive = 'keepalivePositive';
  }
  return errors;
}

/** The `ssh` member of the profile, or `null` while the tunnel is off. */
export function sshConfigFromForm(s: SshFormState): SshConfig | null {
  if (!s.enabled) return null;
  return {
    host: s.host.trim(),
    port: Number(s.port),
    username: s.username.trim(),
    auth:
      s.method === 'privateKey'
        ? { method: 'privateKey', keyPath: s.keyPath.trim(), hasPassphrase: s.hasPassphrase }
        : { method: s.method },
    keepaliveSecs: s.keepaliveSec.trim() ? Number(s.keepaliveSec) : null,
    connectTimeoutMs: null,
  };
}

/** `profile` with the tunnel settings applied (`ssh` is left out when the tunnel is off). */
export function withSsh(profile: ConnectionProfile, s: SshFormState): ConnectionProfile {
  const ssh = sshConfigFromForm(s);
  if (!ssh) {
    const rest = { ...profile };
    delete rest.ssh;
    return rest;
  }
  // Keep the connect timeout the profile already had for the tunnel.
  const connectTimeoutMs = profile.ssh?.connectTimeoutMs ?? null;
  return { ...profile, ssh: { ...ssh, connectTimeoutMs } };
}

/** The stored profile has a keychain entry (`<id>#ssh`) for its tunnel. */
export function hadSshSecret(profile?: ConnectionProfile): boolean {
  const auth = profile?.ssh?.auth;
  if (!auth) return false;
  return auth.method === 'password' || (auth.method === 'privateKey' && auth.hasPassphrase);
}

/** Does this form need a secret at all (a password or a key passphrase)? */
export function needsSshSecret(s: SshFormState): boolean {
  return s.method === 'password' || (s.method === 'privateKey' && s.hasPassphrase);
}

/**
 * Secret change to send with `connectionSave`; `undefined` = leave the stored one alone (the
 * argument is then omitted from the call).
 */
export function sshSecretChange(
  s: SshFormState,
  profile?: ConnectionProfile,
): SecretChange | undefined {
  const stored = hadSshSecret(profile);
  if (!s.enabled || !needsSshSecret(s)) return stored ? { action: 'clear' } : undefined;
  if (s.secret) return { action: 'set', value: s.secret };
  return s.secretAction === 'clear' && stored ? { action: 'clear' } : undefined;
}
