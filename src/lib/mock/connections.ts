import type {
  ConnectionGroup,
  ConnectionProfile,
  ConnectionsSnapshot,
  SecretChange,
  ServerInfo,
  Uuid,
} from '../types';
import { clone, ipcError, newId, sleep } from './util';

const GROUP_WORK = '00000000-0000-4000-8000-0000000000a1';
const now = () => new Date().toISOString();

function profile(
  p: Partial<ConnectionProfile> & Pick<ConnectionProfile, 'id' | 'name' | 'kind'>,
): ConnectionProfile {
  return {
    host: 'localhost',
    port: p.kind === 'mysql' ? 3306 : 5432,
    database: null,
    username: null,
    tls: { mode: 'prefer' },
    connectTimeoutMs: null,
    readOnly: false,
    environment: null,
    options: {},
    groupId: null,
    color: null,
    savePassword: true,
    hasPassword: true,
    sortOrder: 0,
    createdAt: '2026-01-01T00:00:00Z',
    updatedAt: '2026-01-01T00:00:00Z',
    ...p,
  };
}

function seedGroups(): ConnectionGroup[] {
  return [{ id: GROUP_WORK, name: 'Work', parentId: null, sortOrder: 0 }];
}

function seedProfiles(): ConnectionProfile[] {
  return [
    profile({
      id: '00000000-0000-4000-8000-0000000000b1',
      name: 'PostgreSQL dev',
      kind: 'postgres',
      database: 'app_dev',
      username: 'postgres',
      environment: 'development',
      color: '#30a46c',
      groupId: GROUP_WORK,
      sortOrder: 0,
    }),
    profile({
      id: '00000000-0000-4000-8000-0000000000b2',
      name: 'MySQL staging',
      kind: 'mysql',
      host: 'staging-db.internal',
      database: 'shop',
      username: 'app',
      environment: 'staging',
      color: '#f5a524',
      groupId: GROUP_WORK,
      savePassword: false,
      hasPassword: false,
      sortOrder: 1,
    }),
    profile({
      id: '00000000-0000-4000-8000-0000000000b3',
      name: 'PostgreSQL prod (read-only)',
      kind: 'postgres',
      host: 'prod-db.example.com',
      database: 'app',
      username: 'readonly',
      tls: { mode: 'verifyFull', caCertPath: '/etc/ssl/certs/prod-ca.pem' },
      readOnly: true,
      environment: 'production',
      color: '#e5484d',
      sortOrder: 2,
    }),
  ];
}

let groups = seedGroups();
let profiles = seedProfiles();

/** Reset the in-memory store to its seed (used by tests). */
export function resetMockConnections(): void {
  groups = seedGroups();
  profiles = seedProfiles();
}

export function findMockProfile(id: Uuid): ConnectionProfile | undefined {
  return profiles.find((p) => p.id === id);
}

function snapshot(): ConnectionsSnapshot {
  return clone({ groups, profiles });
}

function applySecret(
  existing: ConnectionProfile | undefined,
  p: ConnectionProfile,
  s: SecretChange,
): boolean {
  if (!p.savePassword) return false;
  if (s.action === 'set') return true;
  if (s.action === 'clear') return false;
  return existing?.hasPassword ?? false;
}

export async function connectionsList(): Promise<ConnectionsSnapshot> {
  await sleep();
  return snapshot();
}

export async function connectionSave(
  input: ConnectionProfile,
  secret: SecretChange,
): Promise<ConnectionProfile> {
  await sleep();
  const idx = profiles.findIndex((p) => p.id === input.id);
  const existing = idx >= 0 ? profiles[idx] : undefined;
  const saved: ConnectionProfile = {
    ...clone(input),
    hasPassword: applySecret(existing, input, secret),
    createdAt: existing?.createdAt ?? now(),
    updatedAt: now(),
    sortOrder: existing ? input.sortOrder : profiles.length,
  };
  if (idx >= 0) profiles[idx] = saved;
  else profiles.push(saved);
  return clone(saved);
}

export async function connectionDelete(id: Uuid): Promise<void> {
  await sleep();
  if (!profiles.some((p) => p.id === id)) throw ipcError('notFound', `connection ${id}`);
  profiles = profiles.filter((p) => p.id !== id);
}

export async function connectionDuplicate(id: Uuid): Promise<ConnectionProfile> {
  await sleep();
  const src = findMockProfile(id);
  if (!src) throw ipcError('notFound', `connection ${id}`);
  const copy: ConnectionProfile = {
    ...clone(src),
    id: newId(),
    name: `${src.name} (copy)`,
    hasPassword: false,
    sortOrder: profiles.length,
    createdAt: now(),
    updatedAt: now(),
  };
  profiles.push(copy);
  return clone(copy);
}

export async function groupSave(group: ConnectionGroup): Promise<ConnectionGroup> {
  await sleep();
  const idx = groups.findIndex((g) => g.id === group.id);
  if (idx >= 0) groups[idx] = clone(group);
  else groups.push(clone(group));
  return clone(group);
}

/** Children of a deleted group move to its parent. */
export async function groupDelete(id: Uuid): Promise<void> {
  await sleep();
  const g = groups.find((x) => x.id === id);
  if (!g) throw ipcError('notFound', `group ${id}`);
  const parent = g.parentId ?? null;
  groups = groups
    .filter((x) => x.id !== id)
    .map((x) => (x.parentId === id ? { ...x, parentId: parent } : x));
  profiles = profiles.map((p) => (p.groupId === id ? { ...p, groupId: parent } : p));
}

export function mockServerInfo(kind: ConnectionProfile['kind']): ServerInfo {
  return kind === 'mysql'
    ? { product: 'MySQL', version: '8.4.3', extra: { charset: 'utf8mb4' } }
    : { product: 'PostgreSQL', version: '17.2', extra: { encoding: 'UTF8' } };
}

/** Throws a connection error when the host contains "fail". */
export function assertReachable(p: Pick<ConnectionProfile, 'host' | 'port'>): void {
  if (p.host.toLowerCase().includes('fail')) {
    throw ipcError(
      'connection',
      `could not connect to server at ${p.host}:${p.port}: connection refused`,
    );
  }
}

export async function connectionTest(profileInput: ConnectionProfile): Promise<ServerInfo> {
  await sleep();
  assertReachable(profileInput);
  return mockServerInfo(profileInput.kind);
}

export async function connectionsExport(ids: Uuid[] | null): Promise<number> {
  await sleep();
  return ids ? profiles.filter((p) => ids.includes(p.id)).length : profiles.length;
}

/** The mock "imports" nothing and returns the current snapshot. */
export async function connectionsImport(): Promise<ConnectionsSnapshot> {
  await sleep();
  return snapshot();
}
