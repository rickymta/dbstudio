import { describe, expect, it } from 'vitest';
import type { KeyEntries, Value } from '@/lib/types';
import {
  buildKeyChangeSet,
  deleteKeyChange,
  entryCount,
  expireChange,
  hashDeleteChange,
  hashSetChange,
  listPushChange,
  listRemoveChange,
  listSetChange,
  mergeEntries,
  parseFieldLines,
  renameChange,
  setAddChange,
  setRemoveChange,
  setStringChange,
  streamAddChange,
  streamDeleteChange,
  zAddChange,
  zRemoveChange,
} from './keyChanges';
import { keyPath } from './testUtils';
import { textValue } from './valueFormat';

const k = 'user:1:profile';
const a = textValue('a');
const b = textValue('b');

describe('change builders', () => {
  it('builds a keys ChangeSet addressed to the database of the key', () => {
    const set = buildKeyChangeSet(keyPath(k, '2'), [deleteKeyChange(k)]);
    expect(set).toEqual({
      model: 'keys',
      database: { segments: [{ kind: 'database', name: '2' }] },
      changes: [{ op: 'delete', keys: [k] }],
    });
  });

  it('builds one change per operation of each type', () => {
    expect(setStringChange(k, a)).toEqual({ op: 'setString', key: k, value: a });
    expect(setStringChange(k, a, 5000)).toEqual({ op: 'setString', key: k, value: a, ttlMs: 5000 });
    expect(hashSetChange(k, a, b)).toEqual({ op: 'hashSet', key: k, entries: [[a, b]] });
    expect(hashDeleteChange(k, a)).toEqual({ op: 'hashDelete', key: k, fields: [a] });
    expect(listSetChange(k, 2, a)).toEqual({ op: 'listSet', key: k, index: 2, value: a });
    expect(listPushChange(k, a, true)).toEqual({ op: 'listPush', key: k, values: [a], head: true });
    expect(listRemoveChange(k, 1)).toEqual({ op: 'listRemoveAt', key: k, index: 1 });
    expect(setAddChange(k, a)).toEqual({ op: 'setAdd', key: k, members: [a] });
    expect(setRemoveChange(k, a)).toEqual({ op: 'setRemove', key: k, members: [a] });
    expect(zAddChange(k, a, ' 1.5 ')).toEqual({ op: 'zAdd', key: k, members: [[a, '1.5']] });
    expect(zRemoveChange(k, a)).toEqual({ op: 'zRemove', key: k, members: [a] });
    expect(streamAddChange(k, ' ', [[a, b]])).toEqual({
      op: 'streamAdd',
      key: k,
      id: null,
      fields: [[a, b]],
    });
    expect(streamAddChange(k, '1-1', [[a, b]]).op).toBe('streamAdd');
    expect(streamDeleteChange(k, '1-1')).toEqual({ op: 'streamDelete', key: k, ids: ['1-1'] });
    expect(expireChange(k, null)).toEqual({ op: 'expire', key: k, ttlMs: null });
    expect(renameChange(k, 'n')).toEqual({ op: 'rename', key: k, newKey: 'n' });
  });

  it('keeps the binary key name verbatim', () => {
    const name = '\u0000b64:3q2+7w==';
    expect(deleteKeyChange(name)).toEqual({ op: 'delete', keys: [name] });
  });
});

describe('parseFieldLines', () => {
  it('parses field=value lines', () => {
    expect(parseFieldLines('a=1\n\n b = x=y ')).toEqual([
      [textValue('a'), textValue('1')],
      [textValue('b'), textValue(' x=y')],
    ]);
  });
  it('rejects lines without a field name', () => {
    expect(parseFieldLines('novalue')).toBeNull();
    expect(parseFieldLines('=x')).toBeNull();
    expect(parseFieldLines('')).toBeNull();
  });
});

describe('mergeEntries', () => {
  const text = (v: string): Value => ({ t: 'text', v });
  it('concatenates pages of the same type', () => {
    const hash = mergeEntries(
      { type: 'hash', fields: [[text('a'), text('1')]] },
      { type: 'hash', fields: [[text('b'), text('2')]] },
    );
    expect(hash).toEqual({
      type: 'hash',
      fields: [
        [text('a'), text('1')],
        [text('b'), text('2')],
      ],
    });
    const list = mergeEntries(
      { type: 'list', start: 0, items: [text('x')] },
      { type: 'list', start: 1, items: [text('y')] },
    );
    expect(list).toEqual({ type: 'list', start: 0, items: [text('x'), text('y')] });
  });

  it('counts loaded elements', () => {
    const entries: KeyEntries = { type: 'set', members: [text('a'), text('b')] };
    expect(entryCount(entries)).toBe(2);
    expect(entryCount({ type: 'json', text: '{}' })).toBe(0);
  });
});
