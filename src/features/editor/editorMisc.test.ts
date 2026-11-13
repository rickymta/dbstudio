import { describe, expect, it } from 'vitest';
import { formatSql } from './format';
import { formatterLanguageOf } from './dialects';
import { draftError, draftsFor, toParamValues } from './params';
import { formatKeys, matchesKeys } from './shortcuts';
import { selectRunText } from './runScope';
import type { StatementInfo } from '@/lib/types';

describe('formatSql', () => {
  it('formats with the dialect and keeps parameters', () => {
    const out = formatSql('select a,b from t where id=:id and x=$1', { name: 'postgres' })!;
    expect(out).toContain('SELECT\n  a,\n  b\nFROM\n  t\nWHERE');
    expect(out).toContain(':id');
    expect(out).toContain('$1');
  });
  it('uses the dialect: MySQL backticks, T-SQL brackets', () => {
    expect(
      formatSql('select `a` from `t`', { name: 'mySql', mariadb: false, backslashEscapes: true }),
    ).toContain('`a`');
    expect(formatSql('select [a] from [t]', { name: 'msSql' })).toContain('[a]');
    expect(formatterLanguageOf({ name: 'mySql', mariadb: true, backslashEscapes: true })).toBe(
      'mariadb',
    );
  });
  it('leaves Mongo / Redis text alone', () => {
    expect(formatSql('GET  k', { name: 'redisCli' })).toBe('GET  k');
  });
});

describe('params', () => {
  it('keeps previous values for the same names and validates numbers', () => {
    const refs = [
      { name: ':a', style: 'named' as const, ranges: [] },
      { name: ':b', style: 'named' as const, ranges: [] },
    ];
    const drafts = draftsFor(refs, [{ name: ':a', kind: 'number', value: '5' }]);
    expect(drafts).toEqual([
      { name: ':a', kind: 'number', value: '5' },
      { name: ':b', kind: 'text', value: '' },
    ]);
    for (const ok of ['0', '-1', '3.14', '1e5', '-2.5E-3']) {
      expect(draftError({ name: 'x', kind: 'number', value: ok })).toBeNull();
    }
    for (const bad of ['', '1.', '.5', '1e', 'abc', '1 2']) {
      expect(draftError({ name: 'x', kind: 'number', value: bad })).toBe('params.numberInvalid');
    }
    expect(draftError({ name: 'x', kind: 'raw', value: ' ' })).toBe('params.rawEmpty');
  });
  it('builds the values the backend expects', () => {
    expect(
      toParamValues([
        { name: ':t', kind: 'text', value: "o'k" },
        { name: ':n', kind: 'number', value: ' 7 ' },
        { name: ':b', kind: 'bool', value: 'false' },
        { name: ':z', kind: 'null', value: 'ignored' },
        { name: ':r', kind: 'raw', value: 'now()' },
      ]),
    ).toEqual([
      { name: ':t', value: { kind: 'text', v: "o'k" } },
      { name: ':n', value: { kind: 'number', v: '7' } },
      { name: ':b', value: { kind: 'bool', v: false } },
      { name: ':z', value: { kind: 'null' } },
      { name: ':r', value: { kind: 'raw', v: 'now()' } },
    ]);
  });
});

describe('shortcuts', () => {
  it('formats and matches keys per platform', () => {
    expect(formatKeys('Mod-Shift-f', true)).toBe('⌘⇧F');
    expect(formatKeys('Mod-Shift-f', false)).toBe('Ctrl+Shift+F');
    expect(formatKeys('Mod-Enter', false)).toBe('Ctrl+Enter');
  });
  it('requires the exact modifier set', () => {
    const ev = (init: KeyboardEventInit) => new KeyboardEvent('keydown', init);
    expect(matchesKeys(ev({ key: 't', ctrlKey: true }), 'Mod-t', false)).toBe(true);
    expect(matchesKeys(ev({ key: 't', metaKey: true }), 'Mod-t', false)).toBe(false);
    expect(matchesKeys(ev({ key: 't', metaKey: true }), 'Mod-t', true)).toBe(true);
    expect(matchesKeys(ev({ key: 't', ctrlKey: true, shiftKey: true }), 'Mod-t', false)).toBe(
      false,
    );
    expect(matchesKeys(ev({ key: 'F', ctrlKey: true, shiftKey: true }), 'Mod-Shift-f', false)).toBe(
      true,
    );
    expect(matchesKeys(ev({ key: 't', ctrlKey: true, metaKey: true }), 'Mod-t', false)).toBe(false);
  });
});

describe('selectRunText', () => {
  const st = (index: number, start: number, end: number) =>
    ({ index, range: { start, end } }) as StatementInfo;
  it('maps scopes to text and base offsets, with emoji before the statement', () => {
    const doc = "SELECT '😀'; SELECT 2";
    const statements = [st(0, 0, 10), st(1, 12, 20)];
    expect(selectRunText(doc, 'all', { from: 0, to: 0 }, statements)).toEqual({
      text: doc,
      baseChar: 0,
    });
    // Cursor in the second statement (UTF-16 index 17, scalar 15).
    expect(selectRunText(doc, 'statement', { from: 17, to: 17 }, statements)).toEqual({
      text: 'SELECT 2',
      baseChar: 12,
    });
    // Selection "SELECT 2" at UTF-16 13..21.
    expect(selectRunText(doc, 'selection', { from: 13, to: 21 }, statements)).toEqual({
      text: 'SELECT 2',
      baseChar: 12,
    });
    expect(selectRunText(doc, 'selection', { from: 3, to: 3 }, statements)).toBeNull();
    expect(selectRunText('  ', 'all', { from: 0, to: 0 }, [])).toBeNull();
    expect(selectRunText('', 'statement', { from: 0, to: 0 }, [])).toBeNull();
  });
});
