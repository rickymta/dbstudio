import { describe, expect, it } from 'vitest';
import { NAMESPACES } from '@/lib/i18n';
import { FILTER_OPS, FOLDER_KEYS } from '@/lib/types';

const sources = import.meta.glob<string>(
  ['/src/**/*.{ts,tsx}', '!/src/**/*.test.{ts,tsx}', '!/src/test/**', '!/src/locales/**'],
  { query: '?raw', import: 'default', eager: true },
);
const localeFiles = import.meta.glob<Record<string, unknown>>('./en/*.json', {
  eager: true,
  import: 'default',
});

function flatKeys(obj: unknown, prefix = ''): string[] {
  if (obj === null || typeof obj !== 'object') return [prefix];
  return Object.entries(obj).flatMap(([k, v]) => flatKeys(v, prefix ? `${prefix}.${k}` : k));
}

const available = new Set(
  Object.entries(localeFiles).flatMap(([file, data]) => {
    const ns = /\.\/en\/(\w+)\.json$/.exec(file)![1]!;
    return flatKeys(data).map((k) => `${ns}:${k}`);
  }),
);

/** First namespace given to `useTranslation`, else the default one. */
function defaultNamespace(source: string): string {
  const m = /useTranslation\(\s*\[?\s*['"](\w+)['"]/.exec(source);
  return m?.[1] ?? 'common';
}

function qualify(key: string, fallbackNs: string): string {
  return key.includes(':') ? key : `${fallbackNs}:${key}`;
}

const LITERAL_PATTERNS: { re: RegExp; ns?: (m: RegExpExecArray) => string | undefined }[] = [
  // t('key'), t('ns:key'), i18n.t("key", ...) - literal first argument only
  { re: /\bt\(\s*(['"`])([^'"`$\\]+)\1/g },
  { re: /i18nKey=(['"])([^'"]+)\1/g },
  // { key: 'tabs.welcome', ns: 'common' } tab titles
  { re: /\bkey:\s*(['"])([\w.:-]+)\1(?:\s*,\s*ns:\s*['"](\w+)['"])?/g, ns: (m) => m[3] },
];

/** Families built as t(`prefix.${x}`) whose complete value lists are known. */
const DYNAMIC_FAMILIES: Record<string, readonly string[]> = {
  'explorer:folder': FOLDER_KEYS,
  'explorer:kind': [
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
  ],
  'explorer:action': ['openData', 'viewDdl', 'copyName', 'copyFullName', 'refresh'],
  'data:filterOp': FILTER_OPS,
  'connections:tlsMode': ['disable', 'prefer', 'require', 'verifyCa', 'verifyFull'],
  'connections:environment': ['development', 'staging', 'production'],
  'connections:errorKind': [
    'connection',
    'authentication',
    'query',
    'unsupported',
    'cancelled',
    'timeout',
    'tls',
    'notFound',
    'storage',
    'io',
    'other',
  ],
  'connections:color': [
    'default',
    'green',
    'blue',
    'purple',
    'pink',
    'red',
    'orange',
    'yellow',
    'teal',
  ],
  'connections:validation': ['nameRequired', 'hostRequired', 'portRange', 'timeoutPositive'],
  'common:theme': ['label', 'system', 'light', 'dark'],
  'common:language': ['label', 'vi', 'en'],
};

describe('translation keys used in source', () => {
  it('scans some source files', () => {
    expect(Object.keys(sources).length).toBeGreaterThan(20);
  });

  it('every literal key exists in the English locale files', () => {
    const missing: string[] = [];
    let checked = 0;
    for (const [file, source] of Object.entries(sources)) {
      const fallback = defaultNamespace(source);
      for (const { re, ns } of LITERAL_PATTERNS) {
        for (const m of source.matchAll(re)) {
          const key = m[2]!;
          // Only dotted or namespaced keys are translation keys (skips unrelated t() helpers).
          if (!key.includes('.') && !key.includes(':')) continue;
          const full = qualify(key, ns?.(m as RegExpExecArray) ?? fallback);
          const [prefixNs] = full.split(':');
          if (!(NAMESPACES as readonly string[]).includes(prefixNs!)) continue;
          checked++;
          if (!available.has(full)) missing.push(`${file}: ${full}`);
        }
      }
    }
    expect(checked).toBeGreaterThan(50);
    expect(missing).toEqual([]);
  });

  it('every value of the known dynamic key families is translated', () => {
    const missing = Object.entries(DYNAMIC_FAMILIES).flatMap(([family, values]) =>
      values.map((v) => `${family}.${v}`).filter((k) => !available.has(k)),
    );
    expect(missing).toEqual([]);
  });

  it('dynamic key prefixes in source belong to a known family or an existing prefix', () => {
    const unknown: string[] = [];
    for (const [file, source] of Object.entries(sources)) {
      const fallback = defaultNamespace(source);
      for (const m of source.matchAll(/\bt\(\s*`([^`$]+)\$\{/g)) {
        const prefix = qualify(m[1]!, fallback).replace(/\.$/, '');
        const known =
          prefix in DYNAMIC_FAMILIES || [...available].some((k) => k.startsWith(`${prefix}.`));
        if (!known) unknown.push(`${file}: ${prefix}`);
      }
    }
    expect(unknown).toEqual([]);
  });
});
