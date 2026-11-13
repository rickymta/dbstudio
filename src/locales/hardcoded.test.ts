import { describe, expect, it } from 'vitest';

// Heuristic guard: user-visible text in components goes through i18n, never a literal.
const sources = import.meta.glob<string>(
  ['/src/**/*.tsx', '!/src/**/*.test.tsx', '!/src/test/**'],
  {
    query: '?raw',
    import: 'default',
    eager: true,
  },
);

/** Visible text between tags: two or more words, starting with a capital letter. */
const JSX_TEXT = />\s*([A-Z][A-Za-z']+(?:\s+[A-Za-z']+)+[.!?]?)\s*</g;
/** Accessible/visible string attributes written as plain literals. */
const ATTRIBUTE =
  /\b(?:aria-label|title|placeholder|alt|label)=(["'])([^"'{}\n]*[A-Za-z]{3,}[^"'{}\n]*)\1/g;

/** Literals that are fine: product names, technical identifiers. */
const ALLOWED = new Set(['DBStudio']);
/** Attribute literals that are example input, not prose. */
const EXAMPLE_ATTRIBUTE = /^placeholder=(['"])[\w:*.?[\]-]+\1$/;

describe('hard-coded UI text', () => {
  it('has no English literals in JSX text or accessible attributes', () => {
    const found: string[] = [];
    for (const [file, source] of Object.entries(sources)) {
      for (const m of source.matchAll(JSX_TEXT)) {
        if (!ALLOWED.has(m[1]!)) found.push(`${file}: ${m[1]}`);
      }
      for (const m of source.matchAll(ATTRIBUTE)) {
        if (!ALLOWED.has(m[2]!) && !EXAMPLE_ATTRIBUTE.test(m[0])) found.push(`${file}: ${m[0]}`);
      }
    }
    expect(found).toEqual([]);
  });
});
