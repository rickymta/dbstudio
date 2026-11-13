import { describe, expect, it } from 'vitest';
import en from './en.json';
import vi from './vi.json';

function keys(obj: unknown, prefix = ''): string[] {
  if (obj === null || typeof obj !== 'object') return [prefix];
  return Object.entries(obj).flatMap(([k, v]) => keys(v, prefix ? `${prefix}.${k}` : k));
}

describe('locales', () => {
  it('vi.json and en.json have the same set of keys', () => {
    const enKeys = new Set(keys(en));
    const viKeys = new Set(keys(vi));
    const missingInVi = [...enKeys].filter((k) => !viKeys.has(k));
    const missingInEn = [...viKeys].filter((k) => !enKeys.has(k));
    expect({ missingInVi, missingInEn }).toEqual({ missingInVi: [], missingInEn: [] });
  });
});
