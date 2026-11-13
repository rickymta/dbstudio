import { describe, expect, it } from 'vitest';
import type { ConnectionProfile, SshConfig } from '@/lib/types';
import {
  hadSshSecret,
  initialSshForm,
  needsSshSecret,
  sshConfigFromForm,
  sshSecretChange,
  validateSsh,
  withSsh,
  type SshFormState,
} from './sshForm';

function profile(ssh?: SshConfig | null): ConnectionProfile {
  return {
    id: 'c1',
    name: 'p',
    kind: 'postgres',
    host: 'db',
    port: 5432,
    tls: { mode: 'prefer' },
    readOnly: false,
    options: {},
    ssh,
    savePassword: true,
    hasPassword: false,
    sortOrder: 0,
    createdAt: '',
    updatedAt: '',
  };
}

const pwSsh: SshConfig = {
  host: 'bastion',
  port: 2222,
  username: 'me',
  auth: { method: 'password' },
  keepaliveSecs: 30,
  connectTimeoutMs: 5000,
};

const keySsh: SshConfig = {
  host: 'b',
  port: 22,
  username: 'me',
  auth: { method: 'privateKey', keyPath: '/k', hasPassphrase: true },
};

const on = (patch: Partial<SshFormState> = {}): SshFormState => ({
  ...initialSshForm(),
  enabled: true,
  host: 'b',
  username: 'u',
  ...patch,
});

describe('initialSshForm', () => {
  it('is off with port 22 for a new connection', () => {
    expect(initialSshForm()).toMatchObject({ enabled: false, port: '22', method: 'password' });
    expect(initialSshForm(profile(null)).enabled).toBe(false);
    expect(initialSshForm(profile()).enabled).toBe(false);
  });

  it('loads password, key and agent profiles', () => {
    expect(initialSshForm(profile(pwSsh))).toMatchObject({
      enabled: true,
      host: 'bastion',
      port: '2222',
      username: 'me',
      method: 'password',
      keepaliveSec: '30',
      secret: '',
    });
    expect(initialSshForm(profile(keySsh))).toMatchObject({
      method: 'privateKey',
      keyPath: '/k',
      hasPassphrase: true,
    });
    expect(
      initialSshForm(profile({ ...pwSsh, auth: { method: 'agent' }, keepaliveSecs: null })),
    ).toMatchObject({ method: 'agent', keepaliveSec: '' });
  });
});

describe('validateSsh', () => {
  it('ignores a disabled tunnel', () => {
    expect(validateSsh(initialSshForm())).toEqual({});
  });
  it('requires host, user and a valid port', () => {
    expect(validateSsh({ ...initialSshForm(), enabled: true })).toEqual({
      host: 'hostRequired',
      username: 'usernameRequired',
    });
    expect(validateSsh(on({ port: '0' })).port).toBe('portRange');
    expect(validateSsh(on({ port: '70000' })).port).toBe('portRange');
    expect(validateSsh(on({ port: 'abc' })).port).toBe('portRange');
    expect(validateSsh(on({ port: '22' }))).toEqual({});
  });
  it('requires a key file for key authentication and a positive keep-alive', () => {
    expect(validateSsh(on({ method: 'privateKey' })).keyPath).toBe('keyRequired');
    expect(validateSsh(on({ method: 'privateKey', keyPath: '/k' }))).toEqual({});
    expect(validateSsh(on({ keepaliveSec: '0' })).keepalive).toBe('keepalivePositive');
    expect(validateSsh(on({ keepaliveSec: '15' }))).toEqual({});
    expect(validateSsh(on({ method: 'agent' }))).toEqual({});
  });
});

describe('sshConfigFromForm / withSsh', () => {
  it('builds each authentication method', () => {
    expect(sshConfigFromForm(initialSshForm())).toBeNull();
    expect(sshConfigFromForm(on({ host: ' h ', port: '2200', username: ' u ' }))).toEqual({
      host: 'h',
      port: 2200,
      username: 'u',
      auth: { method: 'password' },
      keepaliveSecs: null,
      connectTimeoutMs: null,
    });
    expect(
      sshConfigFromForm(on({ method: 'privateKey', keyPath: ' /k ', hasPassphrase: true }))!.auth,
    ).toEqual({ method: 'privateKey', keyPath: '/k', hasPassphrase: true });
    expect(sshConfigFromForm(on({ method: 'agent', keepaliveSec: '20' }))).toMatchObject({
      auth: { method: 'agent' },
      keepaliveSecs: 20,
    });
  });

  it('leaves the profile without ssh when off and keeps its timeout when on', () => {
    const p = profile(pwSsh);
    expect('ssh' in withSsh(profile(), initialSshForm())).toBe(false);
    expect('ssh' in withSsh(p, initialSshForm())).toBe(false);
    const edited = withSsh(p, on({ host: 'new' }));
    expect(edited.ssh).toMatchObject({ host: 'new', connectTimeoutMs: 5000 });
  });
});

describe('ssh secret handling', () => {
  it('knows which methods need a secret', () => {
    expect(needsSshSecret(on())).toBe(true);
    expect(needsSshSecret(on({ method: 'agent' }))).toBe(false);
    expect(needsSshSecret(on({ method: 'privateKey', hasPassphrase: false }))).toBe(false);
    expect(needsSshSecret(on({ method: 'privateKey', hasPassphrase: true }))).toBe(true);
  });

  it('detects a stored secret from the profile', () => {
    expect(hadSshSecret(profile(pwSsh))).toBe(true);
    expect(hadSshSecret(profile(keySsh))).toBe(true);
    expect(
      hadSshSecret(
        profile({ ...keySsh, auth: { method: 'privateKey', keyPath: '/k', hasPassphrase: false } }),
      ),
    ).toBe(false);
    expect(hadSshSecret(profile({ ...pwSsh, auth: { method: 'agent' } }))).toBe(false);
    expect(hadSshSecret(profile())).toBe(false);
    expect(hadSshSecret(undefined)).toBe(false);
  });

  it('sets a typed secret, keeps or clears the stored one', () => {
    expect(sshSecretChange(on({ secret: 'pw' }))).toEqual({ action: 'set', value: 'pw' });
    expect(sshSecretChange(on())).toBeUndefined();
    const stored = profile(pwSsh);
    expect(sshSecretChange({ ...initialSshForm(stored) }, stored)).toBeUndefined();
    expect(sshSecretChange({ ...initialSshForm(stored), secretAction: 'clear' }, stored)).toEqual({
      action: 'clear',
    });
    // typing wins over "clear"
    expect(
      sshSecretChange({ ...initialSshForm(stored), secretAction: 'clear', secret: 'n' }, stored),
    ).toEqual({ action: 'set', value: 'n' });
  });

  it('removes the stored secret when the tunnel or its secret goes away', () => {
    const stored = profile(pwSsh);
    expect(sshSecretChange(initialSshForm(), stored)).toEqual({ action: 'clear' });
    expect(sshSecretChange({ ...initialSshForm(stored), method: 'agent' }, stored)).toEqual({
      action: 'clear',
    });
    // nothing stored, nothing to clear
    expect(sshSecretChange(initialSshForm(), profile())).toBeUndefined();
    expect(sshSecretChange(on({ method: 'agent' }), profile())).toBeUndefined();
  });
});
