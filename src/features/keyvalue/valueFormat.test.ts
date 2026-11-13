import { describe, expect, it } from 'vitest';
import {
  bytesToBase64,
  bytesToHex,
  convertDraft,
  draftToValue,
  editedValue,
  hexToBytes,
  initialStringMode,
  prettyJson,
  toDraft,
  valueCell,
  valueText,
} from './valueFormat';

describe('hex and base64', () => {
  it('round-trips bytes', () => {
    const bytes = new Uint8Array([0, 1, 254, 255]);
    expect(bytesToHex(bytes)).toBe('0001feff');
    expect(hexToBytes('00 01 FE ff')).toEqual(bytes);
    expect(hexToBytes('0x0001')).toEqual(new Uint8Array([0, 1]));
    expect(bytesToBase64(bytes)).toBe('AAH+/w==');
  });
  it('rejects bad hex', () => {
    expect(hexToBytes('abc')).toBeNull();
    expect(hexToBytes('zz')).toBeNull();
  });
});

describe('string modes', () => {
  it('opens non-UTF-8 bytes in hex mode', () => {
    expect(initialStringMode({ t: 'bytes', v: bytesToBase64(new Uint8Array([0xff, 0xfe])) })).toBe(
      'hex',
    );
    expect(initialStringMode({ t: 'text', v: 'hi' })).toBe('text');
    expect(
      initialStringMode({ t: 'bytes', v: bytesToBase64(new TextEncoder().encode('hi')) }),
    ).toBe('text');
  });

  it('shows text, pretty JSON and hex', () => {
    const v = { t: 'text', v: '{"a":1}' } as const;
    expect(toDraft(v, 'text')).toBe('{"a":1}');
    expect(toDraft(v, 'json')).toBe('{\n  "a": 1\n}');
    expect(toDraft({ t: 'text', v: 'hi' }, 'hex')).toBe('6869');
    expect(toDraft({ t: 'text', v: 'not json' }, 'json')).toBe('not json');
  });

  it('converts drafts between modes', () => {
    expect(convertDraft('hi', 'text', 'hex')).toBe('6869');
    expect(convertDraft('6869', 'hex', 'text')).toBe('hi');
    expect(convertDraft('ff', 'hex', 'text')).toBeNull();
    expect(convertDraft('zz', 'hex', 'text')).toBeNull();
    expect(convertDraft('{"a":1}', 'text', 'json')).toBe('{\n  "a": 1\n}');
    expect(prettyJson('nope')).toBeNull();
  });

  it('builds the value to save', () => {
    expect(draftToValue('x', 'text')).toEqual({ t: 'text', v: 'x' });
    expect(draftToValue('6869', 'hex')).toEqual({ t: 'bytes', v: 'aGk=' });
    expect(draftToValue('6', 'hex')).toBeNull();
  });
});

describe('cell values', () => {
  it('shows bytes as hex and edits them as hex', () => {
    const bytes = { t: 'bytes', v: 'aGk=' } as const;
    expect(valueText(bytes)).toBe('6869');
    expect(valueCell(bytes)).toBe('0x6869');
    expect(editedValue(bytes, '6a')).toEqual({ t: 'bytes', v: 'ag==' });
    expect(editedValue(bytes, 'xyz')).toBeNull();
    expect(editedValue({ t: 'text', v: 'a' }, 'b')).toEqual({ t: 'text', v: 'b' });
  });
  it('cuts long text', () => {
    expect(valueCell({ t: 'text', v: 'x'.repeat(300) }, 10)).toBe('xxxxxxxxxx...');
  });
});
