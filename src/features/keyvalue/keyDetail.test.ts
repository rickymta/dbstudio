import { describe, expect, it } from 'vitest';
import {
  BINARY_KEY_PREFIX,
  databaseOfKeyPath,
  defaultDeletePattern,
  displayKeyName,
  displayKeyNameRelative,
  escapeGlob,
  formatDuration,
  keyNameOfPath,
  parseKeyDetail,
} from './keyDetail';

describe('parseKeyDetail', () => {
  it('parses a bare type', () => {
    expect(parseKeyDetail('hash')).toEqual({ type: 'hash', ttlSeconds: null });
    expect(parseKeyDetail('ReJSON-RL')).toEqual({ type: 'ReJSON-RL', ttlSeconds: null });
  });

  it('parses type and ttl', () => {
    expect(parseKeyDetail('string ttl:3600')).toEqual({ type: 'string', ttlSeconds: 3600 });
    expect(parseKeyDetail('zset ttl:0')).toEqual({ type: 'zset', ttlSeconds: 0 });
  });

  it('ignores garbage ttl and empty input', () => {
    expect(parseKeyDetail('list ttl:abc')).toEqual({ type: 'list', ttlSeconds: null });
    expect(parseKeyDetail('')).toBeNull();
    expect(parseKeyDetail(null)).toBeNull();
    expect(parseKeyDetail(undefined)).toBeNull();
  });
});

describe('displayKeyName', () => {
  it('keeps normal names', () => {
    expect(displayKeyName('user:1:profile')).toBe('user:1:profile');
  });

  it('shortens binary names', () => {
    expect(displayKeyName(`${BINARY_KEY_PREFIX}3q2+7w==`)).toBe('<binary: 3q2+7w==>');
    expect(displayKeyName(`${BINARY_KEY_PREFIX}AAAAAAAAAAAAAAAAAAAAAAAA`)).toBe(
      '<binary: AAAAAAAAAAAA...>',
    );
  });
});

describe('paths and helpers', () => {
  const path = {
    segments: [
      { kind: 'database' as const, name: '3' },
      { kind: 'keyPrefix' as const, name: 'a:' },
      { kind: 'key' as const, name: 'a:b' },
    ],
  };

  it('extracts database and key name', () => {
    expect(databaseOfKeyPath(path)).toEqual({ segments: [{ kind: 'database', name: '3' }] });
    expect(keyNameOfPath(path)).toBe('a:b');
  });

  it('suggests delete patterns', () => {
    expect(defaultDeletePattern({ kind: 'keyPrefix', name: 'user:1:' })).toBe('user:1:*');
    expect(defaultDeletePattern({ kind: 'database', name: '0' })).toBe('*');
  });

  it('formats durations', () => {
    expect(formatDuration(45)).toBe('45s');
    expect(formatDuration(125)).toBe('2m 5s');
    expect(formatDuration(3600)).toBe('1h');
    expect(formatDuration(3700)).toBe('1h 1m');
    expect(formatDuration(90000)).toBe('1d 1h');
    expect(formatDuration(-5)).toBe('0s');
  });
});

describe('glob escaping and binary names', () => {
  it('escapes glob metacharacters of a prefix in the suggested delete pattern', () => {
    expect(escapeGlob('a*:')).toBe('a\\*:');
    expect(escapeGlob('user[1]:')).toBe('user\\[1\\]:');
    expect(escapeGlob('w?\\')).toBe('w\\?\\\\');
    expect(defaultDeletePattern({ kind: 'keyPrefix', name: 'a*:' })).toBe('a\\*:*');
    expect(defaultDeletePattern({ kind: 'keyPrefix', name: 'user[1]:' })).toBe('user\\[1\\]:*');
  });

  it('suggests nothing for a binary prefix', () => {
    expect(defaultDeletePattern({ kind: 'keyPrefix', name: `${BINARY_KEY_PREFIX}AP8=` })).toBe('');
  });

  it('strips a parent prefix by bytes, also for binary names', () => {
    // bytes 0x61 0x3a ("a:") + 0xff
    const child = `${BINARY_KEY_PREFIX}YTr/`;
    expect(displayKeyNameRelative(child, 'a:')).toMatch(/^<binary:/);
    // Both binary: parent = 0xff 0x3a, child = parent + "x".
    const parent = `${BINARY_KEY_PREFIX}/zo=`;
    const kid = `${BINARY_KEY_PREFIX}/zp4`;
    expect(displayKeyNameRelative(kid, parent)).toBe('x');
    expect(displayKeyNameRelative('a:b', 'a:')).toBe('b');
    expect(displayKeyNameRelative('zzz', 'a:')).toBe('zzz');
  });
});
