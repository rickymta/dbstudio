import { describe, expect, it } from 'vitest';
import { isLoopbackHost, needsTlsWarning } from './tlsWarning';

describe('isLoopbackHost', () => {
  it.each([
    'localhost',
    'LOCALHOST',
    'db.localhost',
    '127.0.0.1',
    '127.0.0.2',
    '127.255.255.254',
    '::1',
    '[::1]',
    ' localhost. ',
  ])('%s is loopback', (h) => expect(isLoopbackHost(h)).toBe(true));

  it.each([
    '',
    '192.168.1.5',
    'example.com',
    'localhost.evil.com',
    'evil-localhost',
    '128.0.0.1',
    '127.0.0.256',
    '127.0.0',
    '::2',
    '10.127.0.1',
  ])('%j is not loopback', (h) => expect(isLoopbackHost(h)).toBe(false));
});

describe('needsTlsWarning', () => {
  it('warns for unverified modes on remote hosts only', () => {
    for (const mode of ['disable', 'prefer', 'require'] as const) {
      expect(needsTlsWarning(mode, 'db.example.com')).toBe(true);
      expect(needsTlsWarning(mode, 'localhost')).toBe(false);
      expect(needsTlsWarning(mode, '')).toBe(false);
    }
    expect(needsTlsWarning('verifyCa', 'db.example.com')).toBe(false);
    expect(needsTlsWarning('verifyFull', 'db.example.com')).toBe(false);
  });
});
