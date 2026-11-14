import { describe, expect, it } from 'vitest';
import en from '@/locales/en/debugger.json';
import vi from '@/locales/vi/debugger.json';

function keys(obj: Record<string, unknown>, prefix = ''): string[] {
  return Object.entries(obj).flatMap(([k, v]) =>
    typeof v === 'object' && v !== null
      ? keys(v as Record<string, unknown>, `${prefix}${k}.`)
      : [`${prefix}${k}`],
  );
}

describe('debugger locales', () => {
  it('have the same keys in vi and en', () => {
    expect(keys(vi).sort()).toEqual(keys(en).sort());
  });
  it('have no empty strings', () => {
    for (const k of [...keys(en), ...keys(vi)]) expect(k).not.toBe('');
    expect(JSON.stringify(en)).not.toContain('""');
    expect(JSON.stringify(vi)).not.toContain('""');
  });
});
