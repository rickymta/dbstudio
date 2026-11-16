import { describe, expect, it } from 'vitest';
import type { ConnectionProfile, SessionInfo } from '@/lib/types';
import { groupConnections } from './pickerLogic';

const profile = (id: string, name: string, kind: ConnectionProfile['kind'], host = 'h') =>
  ({ id, name, kind, host }) as ConnectionProfile;

const profiles = [
  profile('a', 'Alpha', 'postgres', 'db1.local'),
  profile('b', 'Beta', 'mysql'),
  profile('c', 'Gamma', 'mongodb'),
  profile('d', 'Delta', 'postgres'),
  profile('e', 'Cache', 'redis'),
];
const sessions = { b: { sessionId: 's-b' } as SessionInfo, c: { sessionId: 's-c' } as SessionInfo };

describe('groupConnections', () => {
  it('lists open connections first, then the others, only of the editor mode', () => {
    const sql = groupConnections(profiles, sessions, 'sql');
    expect(sql.open.map((p) => p.name)).toEqual(['Beta']);
    expect(sql.other.map((p) => p.name)).toEqual(['Alpha', 'Delta']);
    expect(groupConnections(profiles, sessions, 'mongo').open.map((p) => p.name)).toEqual([
      'Gamma',
    ]);
    expect(groupConnections(profiles, sessions, 'redis').other.map((p) => p.name)).toEqual([
      'Cache',
    ]);
  });

  it('filters by name or host, ignoring case', () => {
    expect(groupConnections(profiles, sessions, 'sql', 'DELT').other.map((p) => p.name)).toEqual([
      'Delta',
    ]);
    expect(groupConnections(profiles, sessions, 'sql', 'db1').other.map((p) => p.name)).toEqual([
      'Alpha',
    ]);
    expect(groupConnections(profiles, sessions, 'sql', 'zzz')).toEqual({ open: [], other: [] });
  });
});
