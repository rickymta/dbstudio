import type { DbKind, ScriptDialect, SessionInfo, Uuid } from '../types';
import { mockDriverFor } from './drivers';
import { assertReachable, findMockProfile, mockServerInfo } from './connections';
import { ipcError, newId, sleep } from './util';

const sessions = new Map<Uuid, { connectionId: Uuid; kind: DbKind }>();

/** Kind of the database behind `sessionId`; throws `notFound` for unknown sessions. */
export function mockSessionKind(sessionId: Uuid): DbKind {
  const s = sessions.get(sessionId);
  if (!s) throw ipcError('notFound', `session ${sessionId}`);
  return s.kind;
}

const DIALECTS: Record<DbKind, ScriptDialect> = {
  postgres: { name: 'postgres' },
  mysql: { name: 'mySql', mariadb: false, backslashEscapes: true },
  sqlserver: { name: 'msSql' },
  oracle: { name: 'oracle' },
  mongodb: { name: 'mongoShell' },
  redis: { name: 'redisCli' },
  cassandra: { name: 'opaque' },
  scylladb: { name: 'opaque' },
  clickhouse: { name: 'opaque' },
};

/** Script dialect of the database behind `sessionId`; throws `notFound` for unknown sessions. */
export function mockSessionDialect(sessionId: Uuid): ScriptDialect {
  return DIALECTS[mockSessionKind(sessionId)];
}

export async function sessionOpen(
  connectionId: Uuid,
  password: string | null,
): Promise<SessionInfo> {
  await sleep();
  const profile = findMockProfile(connectionId);
  if (!profile) throw ipcError('notFound', `connection ${connectionId}`);
  if (!profile.savePassword && !password) {
    throw ipcError('authentication', 'password required');
  }
  assertReachable(profile);
  const sessionId = newId();
  sessions.set(sessionId, { connectionId, kind: profile.kind });
  return {
    sessionId,
    connectionId,
    serverInfo: mockServerInfo(profile.kind),
    descriptor: mockDriverFor(profile.kind),
    dialect: DIALECTS[profile.kind],
  };
}

export async function sessionClose(sessionId: Uuid): Promise<void> {
  await sleep(10, 40);
  sessions.delete(sessionId);
}
