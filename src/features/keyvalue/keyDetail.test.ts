import { describe, expect, it } from 'vitest';
import {
  BINARY_KEY_PREFIX,
  databaseOfKeyPath,
  defaultDeletePattern,
  displayKeyName,
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
