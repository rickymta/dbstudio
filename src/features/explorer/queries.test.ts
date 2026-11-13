import { QueryClient } from '@tanstack/react-query';
import { describe, expect, it } from 'vitest';
import { childPath } from '@/lib/objectPath';
import type { ObjectPath } from '@/lib/types';
import { clearSessionCache, getLevelPattern, setLevelPattern } from './queries';

const db: ObjectPath = { segments: [{ kind: 'database', name: '0' }] };
const prefix = (parent: ObjectPath, name: string) => childPath(parent, 'keyPrefix', name);

describe('level patterns', () => {
  it('are relative to the node and inherited by open sub-folders', () => {
    const qc = new QueryClient();
    const user = prefix(db, 'user:');
    const user1 = prefix(user, 'user:1:');
    const other = prefix(db, 'order:');
    setLevelPattern(qc, 's', user, '1:a*');
    expect(getLevelPattern('s', user)).toBe('1:a*');
    // The literal part covered by the sub-folder is stripped.
    expect(getLevelPattern('s', user1)).toBe('a*');
    // A pattern that does not lead into the sub-folder cannot be translated: no filter.
    expect(getLevelPattern('s', prefix(user, 'user:2:'))).toBe('');
    // Other branches and other sessions are not filtered.
    expect(getLevelPattern('s', other)).toBe('');
    expect(getLevelPattern('t', user1)).toBe('');
    // A leading * stays valid at any depth.
    setLevelPattern(qc, 's', db, '*x');
    expect(getLevelPattern('s', other)).toBe('*x');
    // An own filter wins.
    setLevelPattern(qc, 's', user1, 'q*');
    expect(getLevelPattern('s', user1)).toBe('q*');
    clearSessionCache(qc, 's');
    expect(getLevelPattern('s', user1)).toBe('');
  });
});
