import { describe, expect, it } from 'vitest';
import type { ConnectionGroup, ConnectionProfile } from '@/lib/types';
import {
  buildConnectionsTree,
  filterConnectionsTree,
  groupPathOptions,
  needsPassword,
} from './connectionsTree';
import {
  defaultUsername,
  initialForm,
  profileFromForm,
  saveSecret,
  testSecret,
  validateForm,
} from './connectionForm';

const group = (
  id: string,
  name: string,
  parentId: string | null = null,
  sortOrder = 0,
): ConnectionGroup => ({
  id,
  name,
  parentId,
  sortOrder,
});

function profile(
  id: string,
  name: string,
  groupId: string | null = null,
  sortOrder = 0,
): ConnectionProfile {
  return {
    ...profileFromForm({ ...initialForm(undefined), name, host: 'h' }, undefined, () => id),
    groupId,
    sortOrder,
  };
}

describe('buildConnectionsTree', () => {
  const snapshot = {
    groups: [
      group('b', 'B', 'a'),
      group('a', 'A'),
      group('c', 'C', 'missing'),
      group('x', 'X', 'y'),
      group('y', 'Y', 'x'),
    ],
    profiles: [
      profile('p2', 'Zed', 'a', 1),
      profile('p1', 'Alpha', 'a', 0),
      profile('p3', 'Loose'),
      profile('p4', 'Orphan', 'gone'),
    ],
  };

  it('nests groups, orders by sortOrder then name, and promotes orphans', () => {
    const tree = buildConnectionsTree(snapshot);
    expect(tree.groups.map((g) => g.group.id).sort()).toEqual(['a', 'c', 'x', 'y'].sort());
    const a = tree.groups.find((g) => g.group.id === 'a')!;
    expect(a.groups.map((g) => g.group.id)).toEqual(['b']);
    expect(a.profiles.map((p) => p.id)).toEqual(['p1', 'p2']);
    expect(tree.profiles.map((p) => p.id).sort()).toEqual(['p3', 'p4']);
  });

  it('filters by profile or group name keeping ancestors', () => {
    const tree = buildConnectionsTree(snapshot);
    const byName = (p: ConnectionProfile) => p.name.toLowerCase().includes('alpha');
    const filtered = filterConnectionsTree(tree, 'alpha', byName);
    expect(filtered.profiles).toEqual([]);
    expect(filtered.groups.map((g) => g.group.id)).toEqual(['a']);
    expect(filtered.groups[0]!.profiles.map((p) => p.id)).toEqual(['p1']);
    expect(filtered.groups[0]!.groups).toEqual([]);

    // A matching group name keeps its whole subtree.
    const byGroup = filterConnectionsTree(tree, 'a', () => false);
    expect(byGroup.groups.find((g) => g.group.id === 'a')!.profiles).toHaveLength(2);

    // Without group matching a group is only kept for the profiles it holds.
    const profilesOnly = filterConnectionsTree(tree, 'a', () => false, { matchGroups: false });
    expect(profilesOnly).toEqual({ groups: [], profiles: [] });
  });

  it('builds readable group path options', () => {
    expect(groupPathOptions(snapshot).map((o) => o.label)).toContain('A / B');
  });

  it('never needs a password when the profile has no authentication', () => {
    const none = { ...profile('1', 'n'), authMode: 'none' as const };
    expect(needsPassword({ ...none, savePassword: false, hasPassword: false })).toBe(false);
    expect(needsPassword({ ...none, savePassword: true, hasPassword: false })).toBe(false);
    expect(needsPassword({ ...profile('1', 'n'), authMode: 'password', savePassword: false })).toBe(
      true,
    );
    // Profiles saved before the field existed behave as before.
    const old = { ...profile('1', 'n'), savePassword: false, hasPassword: false };
    delete old.authMode;
    expect(needsPassword(old)).toBe(true);
  });

  it('needs a password unless one is stored', () => {
    expect(needsPassword({ ...profile('1', 'n'), savePassword: true, hasPassword: true })).toBe(
      false,
    );
    expect(needsPassword({ ...profile('1', 'n'), savePassword: false, hasPassword: true })).toBe(
      true,
    );
    expect(needsPassword({ ...profile('1', 'n'), savePassword: true, hasPassword: false })).toBe(
      true,
    );
  });
});

describe('connection form helpers', () => {
  it('validates host, port and timeout (a blank name is allowed)', () => {
    expect(validateForm({ ...initialForm(undefined), name: 'x' })).toEqual({});
    const bad = validateForm({
      ...initialForm(undefined),
      name: ' ',
      host: '',
      port: '70000',
      timeoutSec: '0',
    });
    expect(Object.keys(bad).sort()).toEqual(['host', 'port', 'timeout']);
    expect(validateForm({ ...initialForm(undefined), name: 'x', port: '12a' }).port).toBeDefined();
  });

  it('carries the authentication mode and drops the secret when there is none', () => {
    const base = { ...initialForm(undefined), name: 'n', host: 'h' };
    expect(profileFromForm(base, undefined, () => 'i').authMode).toBe('password');
    const none = { ...base, authMode: 'none' as const, savePassword: true, password: 'pw' };
    const p = profileFromForm(none, undefined, () => 'i');
    expect(p.authMode).toBe('none');
    expect(p.savePassword).toBe(false);
    expect(saveSecret(none)).toEqual({ action: 'clear' });
    expect(testSecret(none)).toEqual({ action: 'clear' });
    expect(initialForm({ ...p, authMode: undefined }).authMode).toBe('password');
    expect(initialForm(p).authMode).toBe('none');
  });

  it('suggests the default login of servers that have one', () => {
    expect(defaultUsername('postgres')).toBe('postgres');
    expect(defaultUsername('clickhouse')).toBe('default');
    expect(defaultUsername('redis')).toBeNull();
    expect(defaultUsername('oracle')).toBeNull();
  });

  it('chooses the secret change', () => {
    const base = initialForm(undefined);
    expect(saveSecret({ ...base, password: 'pw' })).toEqual({ action: 'set', value: 'pw' });
    expect(saveSecret(base)).toEqual({ action: 'keep' });
    expect(saveSecret({ ...base, passwordAction: 'clear' })).toEqual({ action: 'clear' });
    expect(saveSecret({ ...base, password: 'pw', savePassword: false })).toEqual({
      action: 'clear',
    });
    expect(testSecret({ ...base, password: 'pw', savePassword: false })).toEqual({
      action: 'set',
      value: 'pw',
    });
  });

  it('only keeps certificate paths for verifying TLS modes', () => {
    const form = {
      ...initialForm(undefined),
      name: 'x',
      caCertPath: '/ca.pem',
      tlsMode: 'require' as const,
    };
    expect(profileFromForm(form, undefined, () => 'id').tls.caCertPath).toBeNull();
    expect(
      profileFromForm({ ...form, tlsMode: 'verifyCa' }, undefined, () => 'id').tls.caCertPath,
    ).toBe('/ca.pem');
  });
});
