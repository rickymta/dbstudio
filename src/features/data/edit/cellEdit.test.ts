import { describe, expect, it } from 'vitest';
import type { ColumnMeta } from '@/lib/types';
import {
  editDisplayValue,
  editEqualsOriginal,
  editorKind,
  isNullEdit,
  needsDialogEditor,
  NULL_EDIT,
  parseInput,
} from './cellEdit';

describe('NULL is not the empty string', () => {
  it('typed empty text is a text value, never NULL', () => {
    const r = parseInput('text', '', false);
    expect(r).toEqual({ ok: true, edit: { kind: 'text', v: '' } });
    if (!r.ok) throw new Error('expected ok');
    expect(isNullEdit(r.edit)).toBe(false);
  });

  it('asNull wins over the text and works for every kind', () => {
    for (const kind of ['text', 'int', 'json', 'bool', 'timestamp'] as const) {
      expect(parseInput(kind, 'whatever', true)).toEqual({ ok: true, edit: NULL_EDIT });
    }
  });

  it('a cell holding NULL is changed by the empty text, and the other way round', () => {
    expect(editEqualsOriginal({ t: 'null' }, { kind: 'text', v: '' })).toBe(false);
    expect(editEqualsOriginal({ t: 'text', v: '' }, NULL_EDIT)).toBe(false);
    expect(editEqualsOriginal({ t: 'null' }, NULL_EDIT)).toBe(true);
    expect(editEqualsOriginal({ t: 'text', v: '' }, { kind: 'text', v: '' })).toBe(true);
  });

  it('shows NULL and the empty text differently', () => {
    expect(editDisplayValue(NULL_EDIT, 'text')).toEqual({ t: 'null' });
    expect(editDisplayValue({ kind: 'text', v: '' }, 'text')).toEqual({ t: 'text', v: '' });
    expect(editDisplayValue({ kind: 'default' }, 'text')).toBeNull();
  });
});

describe('parseInput', () => {
  it('keeps 64-bit integers and decimals exact as text', () => {
    expect(parseInput('int', ' 9007199254740993 ', false)).toEqual({
      ok: true,
      edit: { kind: 'text', v: '9007199254740993' },
    });
    expect(parseInput('decimal', '12345678901234567890.123456789', false)).toEqual({
      ok: true,
      edit: { kind: 'text', v: '12345678901234567890.123456789' },
    });
  });

  it('rejects malformed numbers, JSON and booleans', () => {
    expect(parseInput('int', '1.5', false)).toEqual({ ok: false, error: 'invalidNumber' });
    expect(parseInput('uInt', '-1', false)).toEqual({ ok: false, error: 'invalidNumber' });
    expect(parseInput('float', '1e5', false).ok).toBe(true);
    expect(parseInput('float', 'NaN', false).ok).toBe(true);
    expect(parseInput('decimal', 'NaN', false)).toEqual({ ok: false, error: 'invalidNumber' });
    expect(parseInput('json', '{"a":', false)).toEqual({ ok: false, error: 'invalidJson' });
    expect(parseInput('json', '{"a": [1, 2]}', false)).toEqual({
      ok: true,
      edit: { kind: 'text', v: '{"a": [1, 2]}' },
    });
    expect(parseInput('bool', 'maybe', false)).toEqual({ ok: false, error: 'invalidBool' });
    expect(parseInput('bool', 'FALSE', false)).toEqual({
      ok: true,
      edit: { kind: 'value', v: { t: 'bool', v: false } },
    });
  });

  it('passes dates and times through untouched', () => {
    expect(parseInput('dateTime', '2024-02-29 13:45:01.123456', false)).toEqual({
      ok: true,
      edit: { kind: 'text', v: '2024-02-29 13:45:01.123456' },
    });
  });
});

describe('editEqualsOriginal', () => {
  it('compares typed text with the stored value', () => {
    expect(editEqualsOriginal({ t: 'int', v: 5 }, { kind: 'text', v: '5' })).toBe(true);
    expect(
      editEqualsOriginal(
        { t: 'int', v: '9007199254740993' },
        { kind: 'text', v: '9007199254740993' },
      ),
    ).toBe(true);
    expect(editEqualsOriginal({ t: 'int', v: 5 }, { kind: 'text', v: '6' })).toBe(false);
    expect(
      editEqualsOriginal({ t: 'bool', v: true }, { kind: 'value', v: { t: 'bool', v: true } }),
    ).toBe(true);
    expect(editEqualsOriginal({ t: 'text', v: 'a' }, { kind: 'default' })).toBe(false);
  });
});

describe('editor selection', () => {
  const col = (valueKind: ColumnMeta['valueKind']): ColumnMeta => ({
    name: 'c',
    typeName: 't',
    valueKind,
  });

  it('maps value kinds to editors', () => {
    expect(editorKind('bool')).toBe('bool');
    expect(editorKind('decimal')).toBe('number');
    expect(editorKind('json')).toBe('json');
    expect(editorKind('timestamp')).toBe('temporal');
    expect(editorKind('bytes')).toBe('binary');
    expect(editorKind('uuid')).toBe('text');
  });

  it('uses the dialog for JSON, binary and long or multi-line text only', () => {
    expect(needsDialogEditor(col('json'), null)).toBe(true);
    expect(needsDialogEditor(col('bytes'), null)).toBe(true);
    expect(needsDialogEditor(col('text'), { t: 'text', v: 'short' })).toBe(false);
    expect(needsDialogEditor(col('text'), { t: 'text', v: 'a\nb' })).toBe(true);
    expect(needsDialogEditor(col('text'), { t: 'text', v: 'x'.repeat(200) })).toBe(true);
    expect(needsDialogEditor(col('int'), { t: 'int', v: 1 })).toBe(false);
  });
});
