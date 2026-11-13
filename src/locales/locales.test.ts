import { describe, expect, it } from 'vitest';
import { NAMESPACES } from '@/lib/i18n';
import { FILTER_OPS, FOLDER_KEYS } from '@/lib/types';
import type { ObjectKind } from '@/lib/types';

const files = import.meta.glob<Record<string, unknown>>('./*/*.json', {
  eager: true,
  import: 'default',
});

function load(lang: string, ns: string): Record<string, unknown> {
  const data = files[`./${lang}/${ns}.json`];
  if (!data) throw new Error(`missing locale file ${lang}/${ns}.json`);
  return data;
}

function keys(obj: unknown, prefix = ''): string[] {
  if (obj === null || typeof obj !== 'object') return [prefix];
  return Object.entries(obj).flatMap(([k, v]) => keys(v, prefix ? `${prefix}.${k}` : k));
}

describe('locales', () => {
  it.each(NAMESPACES)('vi and en have the same keys in namespace %s', (ns) => {
    const enKeys = new Set(keys(load('en', ns)));
    const viKeys = new Set(keys(load('vi', ns)));
    const missingInVi = [...enKeys].filter((k) => !viKeys.has(k));
    const missingInEn = [...viKeys].filter((k) => !enKeys.has(k));
    expect({ missingInVi, missingInEn }).toEqual({ missingInVi: [], missingInEn: [] });
  });

  it('has no files outside the declared namespaces', () => {
    const found = Object.keys(files).sort();
    const expected = ['en', 'vi'].flatMap((l) => NAMESPACES.map((n) => `./${l}/${n}.json`)).sort();
    expect(found).toEqual(expected);
  });

  it('translates every folder key, object kind, filter operator, TLS mode and environment', () => {
    const kinds: ObjectKind[] = [
      'database',
      'schema',
      'table',
      'view',
      'materializedView',
      'role',
      'column',
      'index',
      'function',
      'procedure',
      'trigger',
      'sequence',
      'collection',
      'key',
      'folder',
    ];
    const expectKeys = (lang: string) => {
      const explorer = keys(load(lang, 'explorer'));
      const data = keys(load(lang, 'data'));
      const conn = keys(load(lang, 'connections'));
      return [
        ...FOLDER_KEYS.map((k) => `explorer:folder.${k}`),
        ...kinds.map((k) => `explorer:kind.${k}`),
        ...FILTER_OPS.map((k) => `data:filterOp.${k}`),
        ...['disable', 'prefer', 'require', 'verifyCa', 'verifyFull'].map(
          (k) => `connections:tlsMode.${k}`,
        ),
        ...['development', 'staging', 'production'].map((k) => `connections:environment.${k}`),
      ].filter((full) => {
        const [ns, key] = full.split(':') as [string, string];
        return !{ explorer, data, connections: conn }[ns as 'explorer']!.includes(key);
      });
    };
    expect(expectKeys('en')).toEqual([]);
    expect(expectKeys('vi')).toEqual([]);
  });
});
